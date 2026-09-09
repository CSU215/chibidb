use crate::ast::DataType;
use crate::{Error, Result};

const MAGIC: [u8; 8] = *b"CHIDCAT2";

const DTYPE_INT: u8 = 0x00;
const DTYPE_FLOAT: u8 = 0x01;
const DTYPE_CHAR: u8 = 0x02;
const DTYPE_DATE: u8 = 0x03;
const DTYPE_TEXT: u8 = 0x04;

#[derive(Debug, Clone, PartialEq)]
pub struct TableMeta {
    pub name: String,
    pub columns: Vec<(String, DataType)>,
    pub file_no: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexMeta {
    pub name: String,
    pub table: String,
    pub column: String,
    pub file_no: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatalogSnapshot {
    pub next_table_file: u32,
    pub next_index_file: u32,
    pub tables: Vec<TableMeta>,
    pub indexes: Vec<IndexMeta>,
}

pub fn encode_catalog(snap: &CatalogSnapshot) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&MAGIC);
    put_u32(&mut buf, snap.next_table_file);
    put_u32(&mut buf, snap.next_index_file);
    put_u32(&mut buf, snap.tables.len() as u32);
    for t in &snap.tables {
        put_str(&mut buf, &t.name);
        put_u32(&mut buf, t.columns.len() as u32);
        for (name, dtype) in &t.columns {
            put_str(&mut buf, name);
            match dtype {
                DataType::Int => buf.push(DTYPE_INT),
                DataType::Float => buf.push(DTYPE_FLOAT),
                DataType::Char(n) => {
                    buf.push(DTYPE_CHAR);
                    put_u32(&mut buf, *n);
                }
                DataType::Date => buf.push(DTYPE_DATE),
                DataType::Text => buf.push(DTYPE_TEXT),
            }
        }
        put_u32(&mut buf, t.file_no);
    }
    put_u32(&mut buf, snap.indexes.len() as u32);
    for ix in &snap.indexes {
        put_str(&mut buf, &ix.name);
        put_str(&mut buf, &ix.table);
        put_str(&mut buf, &ix.column);
        put_u32(&mut buf, ix.file_no);
    }
    buf
}

pub fn decode_catalog(data: &[u8]) -> Result<CatalogSnapshot> {
    if data.len() < MAGIC.len() || data[0..MAGIC.len()] != MAGIC {
        return Err(Error::Runtime("not a chibidb catalog file".into()));
    }
    let mut pos = MAGIC.len();
    let next_table_file = take_u32(data, &mut pos)?;
    let next_index_file = take_u32(data, &mut pos)?;
    let n_tables = take_u32(data, &mut pos)?;
    let mut tables = Vec::new();
    for _ in 0..n_tables {
        let name = take_str(data, &mut pos)?;
        let n_cols = take_u32(data, &mut pos)? as usize;
        let mut columns = Vec::with_capacity(n_cols);
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
            columns.push((cname, dtype));
        }
        let file_no = take_u32(data, &mut pos)?;
        tables.push(TableMeta { name, columns, file_no });
    }
    let n_indexes = take_u32(data, &mut pos)?;
    let mut indexes = Vec::new();
    for _ in 0..n_indexes {
        let name = take_str(data, &mut pos)?;
        let table = take_str(data, &mut pos)?;
        let column = take_str(data, &mut pos)?;
        let file_no = take_u32(data, &mut pos)?;
        indexes.push(IndexMeta { name, table, column, file_no });
    }
    if pos != data.len() {
        return Err(Error::Runtime("trailing bytes in catalog file".into()));
    }
    Ok(CatalogSnapshot { next_table_file, next_index_file, tables, indexes })
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
