use crate::ast::DataType;
use crate::config::EngineKind;
use crate::storage::codec::{decode_row, encode_row};
use crate::storage::header::{self, FileKind};
use crate::value::Value;
use crate::{Error, Result};

const MAGIC: [u8; 8] = *b"CHIDCAT7";

const DTYPE_INT: u8 = 0x00;
const DTYPE_FLOAT: u8 = 0x01;
const DTYPE_CHAR: u8 = 0x02;
const DTYPE_DATE: u8 = 0x03;
const DTYPE_TEXT: u8 = 0x04;

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnMeta {
    pub name: String,
    pub dtype: DataType,
    pub not_null: bool,
    pub primary_key: bool,
    pub unique: bool,
    pub default: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableMeta {
    pub name: String,
    pub columns: Vec<ColumnMeta>,
    pub file_no: u32,
    pub engine: EngineKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexMeta {
    pub name: String,
    pub table: String,
    pub column: String,
    pub unique: bool,
    pub file_no: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewMeta {
    pub name: String,
    pub sql: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatalogSnapshot {
    pub next_table_file: u32,
    pub next_index_file: u32,
    pub next_trx_id: u32,
    pub committed_trxs: Vec<u32>,
    pub tables: Vec<TableMeta>,
    pub indexes: Vec<IndexMeta>,
    pub views: Vec<ViewMeta>,
}

pub fn encode_catalog(snap: &CatalogSnapshot) -> Vec<u8> {
    let mut buf = vec![0u8; header::HEADER_LEN];
    header::write_header(&mut buf, &MAGIC, FileKind::Catalog);
    put_u32(&mut buf, snap.next_table_file);
    put_u32(&mut buf, snap.next_index_file);
    put_u32(&mut buf, snap.next_trx_id);
    put_u32(&mut buf, snap.committed_trxs.len() as u32);
    for id in &snap.committed_trxs {
        put_u32(&mut buf, *id);
    }
    put_u32(&mut buf, snap.tables.len() as u32);
    for t in &snap.tables {
        put_str(&mut buf, &t.name);
        put_u32(&mut buf, t.columns.len() as u32);
        for c in &t.columns {
            put_str(&mut buf, &c.name);
            match c.dtype {
                DataType::Int => buf.push(DTYPE_INT),
                DataType::Float => buf.push(DTYPE_FLOAT),
                DataType::Char(n) => {
                    buf.push(DTYPE_CHAR);
                    put_u32(&mut buf, n);
                }
                DataType::Date => buf.push(DTYPE_DATE),
                DataType::Text => buf.push(DTYPE_TEXT),
            }
            buf.push(c.not_null as u8);
            buf.push(c.primary_key as u8);
            buf.push(c.unique as u8);
            put_value(&mut buf, c.default.as_ref());
        }
        put_u32(&mut buf, t.file_no);
        buf.push(engine_tag(t.engine));
    }
    put_u32(&mut buf, snap.indexes.len() as u32);
    for ix in &snap.indexes {
        put_str(&mut buf, &ix.name);
        put_str(&mut buf, &ix.table);
        put_str(&mut buf, &ix.column);
        buf.push(ix.unique as u8);
        put_u32(&mut buf, ix.file_no);
    }
    put_u32(&mut buf, snap.views.len() as u32);
    for v in &snap.views {
        put_str(&mut buf, &v.name);
        put_str(&mut buf, &v.sql);
    }
    buf
}

pub fn decode_catalog(data: &[u8]) -> Result<CatalogSnapshot> {
    header::read_header(data, &MAGIC, FileKind::Catalog)?;
    let mut pos = header::HEADER_LEN;
    let next_table_file = take_u32(data, &mut pos)?;
    let next_index_file = take_u32(data, &mut pos)?;
    let next_trx_id = take_u32(data, &mut pos)?;
    let n_committed = take_u32(data, &mut pos)? as usize;
    // Do not pre-allocate from a file-supplied count: a corrupt catalog could
    // otherwise request a huge allocation before the reads fail.
    let mut committed_trxs = Vec::new();
    for _ in 0..n_committed {
        committed_trxs.push(take_u32(data, &mut pos)?);
    }
    let n_tables = take_u32(data, &mut pos)?;
    let mut tables = Vec::new();
    for _ in 0..n_tables {
        let name = take_str(data, &mut pos)?;
        let n_cols = take_u32(data, &mut pos)? as usize;
        let mut columns = Vec::new();
        for _ in 0..n_cols {
            let cname = take_str(data, &mut pos)?;
            let tag = take(data, &mut pos, 1)?[0];
            let dtype = match tag {
                DTYPE_INT => DataType::Int,
                DTYPE_FLOAT => DataType::Float,
                DTYPE_CHAR => DataType::Char(take_u32(data, &mut pos)?),
                DTYPE_DATE => DataType::Date,
                DTYPE_TEXT => DataType::Text,
                _ => return Err(Error::Runtime(format!("unknown dtype tag 0x{tag:02x}"))),
            };
            let not_null = take(data, &mut pos, 1)?[0] != 0;
            let primary_key = take(data, &mut pos, 1)?[0] != 0;
            let unique = take(data, &mut pos, 1)?[0] != 0;
            let default = take_value(data, &mut pos)?;
            columns.push(ColumnMeta { name: cname, dtype, not_null, primary_key, unique, default });
        }
        let file_no = take_u32(data, &mut pos)?;
        let engine = match take(data, &mut pos, 1)?[0] {
            0 => EngineKind::Heap,
            1 => EngineKind::Lsm,
            other => return Err(Error::Runtime(format!("unknown engine tag 0x{other:02x}"))),
        };
        tables.push(TableMeta { name, columns, file_no, engine });
    }
    let n_indexes = take_u32(data, &mut pos)?;
    let mut indexes = Vec::new();
    for _ in 0..n_indexes {
        let name = take_str(data, &mut pos)?;
        let table = take_str(data, &mut pos)?;
        let column = take_str(data, &mut pos)?;
        let unique = take(data, &mut pos, 1)?[0] != 0;
        let file_no = take_u32(data, &mut pos)?;
        indexes.push(IndexMeta { name, table, column, unique, file_no });
    }
    let n_views = take_u32(data, &mut pos)?;
    let mut views = Vec::new();
    for _ in 0..n_views {
        let name = take_str(data, &mut pos)?;
        let sql = take_str(data, &mut pos)?;
        views.push(ViewMeta { name, sql });
    }
    if pos != data.len() {
        return Err(Error::Runtime("trailing bytes in catalog file".into()));
    }
    Ok(CatalogSnapshot {
        next_table_file,
        next_index_file,
        next_trx_id,
        committed_trxs,
        tables,
        indexes,
        views,
    })
}

fn put_value(buf: &mut Vec<u8>, v: Option<&Value>) {
    match v {
        None => buf.push(0),
        Some(val) => {
            buf.push(1);
            let bytes = encode_row(std::slice::from_ref(val));
            put_u32(buf, bytes.len() as u32);
            buf.extend_from_slice(&bytes);
        }
    }
}

fn take_value(data: &[u8], pos: &mut usize) -> Result<Option<Value>> {
    if take(data, pos, 1)?[0] == 0 {
        return Ok(None);
    }
    let len = take_u32(data, pos)? as usize;
    let bytes = take(data, pos, len)?;
    let (row, _) = decode_row(bytes)?;
    Ok(row.into_iter().next())
}

fn engine_tag(engine: EngineKind) -> u8 {
    match engine {
        EngineKind::Heap => 0,
        EngineKind::Lsm => 1,
    }
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    put_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
}

fn take<'a>(data: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    if *pos + n > data.len() {
        return Err(Error::Runtime("truncated catalog file".into()));
    }
    let s = &data[*pos..*pos + n];
    *pos += n;
    Ok(s)
}

fn take_u32(data: &[u8], pos: &mut usize) -> Result<u32> {
    let b = take(data, pos, 4)?;
    Ok(u32::from_le_bytes(b.try_into().unwrap()))
}

fn take_str(data: &[u8], pos: &mut usize) -> Result<String> {
    let len = take_u32(data, pos)? as usize;
    let bytes = take(data, pos, len)?;
    String::from_utf8(bytes.to_vec())
        .map_err(|_| Error::Runtime("invalid utf8 in catalog file".into()))
}
