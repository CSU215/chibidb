use crate::storage::lob::LobStore;
use crate::value::Value;
use crate::{Error, Result};

const TAG_NULL: u8 = 0x00;
const TAG_INT: u8 = 0x01;
const TAG_FLOAT: u8 = 0x02;
const TAG_STR: u8 = 0x03;
const TAG_BOOL: u8 = 0x04;
const TAG_DATE: u8 = 0x05;
/// A reference to an out-of-line large object (a `u64` id follows).
const TAG_LOB: u8 = 0x06;

/// Out-of-line storage used to externalize long string values.
pub trait LobResolver {
    fn put(&self, data: &[u8]) -> Result<u64>;
    fn get(&self, id: u64) -> Result<Vec<u8>>;
}

impl LobResolver for LobStore {
    fn put(&self, data: &[u8]) -> Result<u64> {
        self.write(data)
    }

    fn get(&self, id: u64) -> Result<Vec<u8>> {
        self.read(id)
    }
}

/// Versioned record: two hidden u32 transaction fields precede the row. String
/// values longer than `inline_limit` are stored out-of-line in `lobs`.
pub fn encode_record(
    creator: u32,
    deleter: u32,
    row: &[Value],
    lobs: &dyn LobResolver,
    inline_limit: usize,
) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&creator.to_le_bytes());
    buf.extend_from_slice(&deleter.to_le_bytes());
    buf.extend(encode_row_with(row, Some(lobs), inline_limit)?);
    Ok(buf)
}

/// Reads the `(creator, deleter)` version header of a versioned record.
pub fn record_version(data: &[u8]) -> Result<(u32, u32)> {
    if data.len() < 8 {
        return Err(Error::Runtime("truncated versioned record".into()));
    }
    let creator = u32::from_le_bytes(data[0..4].try_into().unwrap());
    let deleter = u32::from_le_bytes(data[4..8].try_into().unwrap());
    Ok((creator, deleter))
}

pub fn decode_record(
    data: &[u8],
    lobs: &dyn LobResolver,
) -> Result<(u32, u32, Vec<Value>)> {
    let (creator, deleter) = record_version(data)?;
    let (row, _) = decode_row_with(&data[8..], Some(lobs), None)?;
    Ok((creator, deleter, row))
}

/// Like [`decode_record`], but skips resolving large objects in columns whose
/// `keep` entry is `false` (such a column becomes NULL). Callers must only set
/// `false` for columns the query never reads.
pub fn decode_record_pruned(
    data: &[u8],
    lobs: &dyn LobResolver,
    keep: &[bool],
) -> Result<(u32, u32, Vec<Value>)> {
    let (creator, deleter) = record_version(data)?;
    let (row, _) = decode_row_with(&data[8..], Some(lobs), Some(keep))?;
    Ok((creator, deleter, row))
}

/// Collects the large-object ids referenced by an encoded record, without
/// resolving them. Best-effort: malformed bytes end the scan. Used to free an
/// object when its last version is physically removed.
pub fn collect_lob_ids(data: &[u8]) -> Vec<u64> {
    let mut ids = Vec::new();
    if data.len() >= 8 {
        collect_row_lob_ids(&data[8..], &mut ids);
    }
    ids
}

fn collect_row_lob_ids(data: &[u8], out: &mut Vec<u64>) {
    if data.len() < 2 {
        return;
    }
    let count = u16::from_le_bytes([data[0], data[1]]) as usize;
    let mut pos = 2;
    for _ in 0..count {
        let Some(&tag) = data.get(pos) else { return };
        pos += 1;
        match tag {
            TAG_NULL => {}
            TAG_INT | TAG_FLOAT => pos += 8,
            TAG_BOOL => pos += 1,
            TAG_DATE => pos += 4,
            TAG_STR => {
                if pos + 2 > data.len() {
                    return;
                }
                let len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
                pos += 2 + len;
            }
            TAG_LOB => {
                if pos + 8 > data.len() {
                    return;
                }
                out.push(u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap()));
                pos += 8;
            }
            _ => return,
        }
        if pos > data.len() {
            return;
        }
    }
}

/// Byte ranges of each tagged value inside a row encoding. The leading `u16`
/// column count is not part of any range. Lets the PAX page codec slice a
/// stored record into columns without decoding the values.
pub(crate) fn row_column_ranges(data: &[u8]) -> Result<Vec<(usize, usize)>> {
    if data.len() < 2 {
        return Err(Error::Runtime("truncated row data".into()));
    }
    let count = u16::from_le_bytes([data[0], data[1]]) as usize;
    let mut ranges = Vec::with_capacity(count);
    let mut pos = 2;
    for _ in 0..count {
        let start = pos;
        let tag = *data
            .get(pos)
            .ok_or_else(|| Error::Runtime("truncated row data".into()))?;
        pos += 1;
        pos += match tag {
            TAG_NULL => 0,
            TAG_INT | TAG_FLOAT | TAG_LOB => 8,
            TAG_BOOL => 1,
            TAG_DATE => 4,
            TAG_STR => {
                let lb = take(data, &mut pos, 2)?;
                u16::from_le_bytes(lb.try_into().unwrap()) as usize
            }
            _ => return Err(Error::Runtime(format!("unknown value tag 0x{tag:02x}"))),
        };
        if pos > data.len() {
            return Err(Error::Runtime("truncated row data".into()));
        }
        ranges.push((start, pos));
    }
    Ok(ranges)
}

/// Size (excluding the version header) an externalized row encoding will have,
/// without writing any large object.
pub fn encoded_row_size(row: &[Value], inline_limit: usize) -> usize {
    let mut size = 2;
    for v in row {
        size += match v {
            Value::Null => 1,
            Value::Int(_) | Value::Float(_) => 9,
            Value::Bool(_) => 2,
            Value::Date(_) => 5,
            Value::Str(s) => {
                if s.len() > inline_limit {
                    9 // tag + u64 lob id
                } else {
                    3 + s.len() // tag + u16 length + bytes
                }
            }
        };
    }
    size
}

/// Encodes a versioned record with every string inline, for tests and callers
/// that do not use large objects.
pub fn encode_record_inline(creator: u32, deleter: u32, row: &[Value]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&creator.to_le_bytes());
    buf.extend_from_slice(&deleter.to_le_bytes());
    buf.extend(encode_row(row));
    buf
}

/// Encodes a row with every string inline (used for catalog metadata).
pub fn encode_row(row: &[Value]) -> Vec<u8> {
    encode_row_with(row, None, 0).expect("inline row encoding never fails")
}

pub fn decode_row(data: &[u8]) -> Result<(Vec<Value>, usize)> {
    decode_row_with(data, None, None)
}

fn encode_row_with(
    row: &[Value],
    lobs: Option<&dyn LobResolver>,
    inline_limit: usize,
) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(row.len() as u16).to_le_bytes());
    for v in row {
        match v {
            Value::Null => buf.push(TAG_NULL),
            Value::Int(n) => {
                buf.push(TAG_INT);
                buf.extend_from_slice(&n.to_le_bytes());
            }
            Value::Float(x) => {
                buf.push(TAG_FLOAT);
                buf.extend_from_slice(&x.to_le_bytes());
            }
            Value::Str(s) => {
                if let Some(lobs) = lobs
                    && s.len() > inline_limit
                {
                    buf.push(TAG_LOB);
                    buf.extend_from_slice(&lobs.put(s.as_bytes())?.to_le_bytes());
                } else {
                    buf.push(TAG_STR);
                    buf.extend_from_slice(&(s.len() as u16).to_le_bytes());
                    buf.extend_from_slice(s.as_bytes());
                }
            }
            Value::Bool(b) => {
                buf.push(TAG_BOOL);
                buf.push(*b as u8);
            }
            Value::Date(d) => {
                buf.push(TAG_DATE);
                buf.extend_from_slice(&d.to_le_bytes());
            }
        }
    }
    Ok(buf)
}

fn decode_row_with(
    data: &[u8],
    lobs: Option<&dyn LobResolver>,
    keep: Option<&[bool]>,
) -> Result<(Vec<Value>, usize)> {
    let mut pos = 0;
    let hb = take(data, &mut pos, 2)?;
    let count = u16::from_le_bytes(hb.try_into().unwrap()) as usize;
    let mut row = Vec::with_capacity(count);
    for i in 0..count {
        row.push(decode_value(data, &mut pos, i, lobs, keep)?);
    }
    Ok((row, pos))
}

/// Decodes an encoded row, handing each value to `sink` as it is read instead
/// of collecting a `Vec<Value>`. Returns the column count. Lets a scan fill
/// chunk columns directly.
pub(crate) fn decode_row_each(    data: &[u8],
    lobs: Option<&dyn LobResolver>,
    keep: Option<&[bool]>,
    mut sink: impl FnMut(usize, Value) -> Result<()>,
) -> Result<usize> {
    let mut pos = 0;
    let hb = take(data, &mut pos, 2)?;
    let count = u16::from_le_bytes(hb.try_into().unwrap()) as usize;
    for i in 0..count {
        sink(i, decode_value(data, &mut pos, i, lobs, keep)?)?;
    }
    Ok(count)
}

/// Like [`decode_row_each`], but collects the values into `out` (cleared
/// first), reusing its allocation. Returns the column count.
pub(crate) fn decode_row_into(
    data: &[u8],
    lobs: Option<&dyn LobResolver>,
    keep: Option<&[bool]>,
    out: &mut Vec<Value>,
) -> Result<usize> {
    out.clear();
    decode_row_each(data, lobs, keep, |_, value| {
        out.push(value);
        Ok(())
    })
}

/// Decodes one tagged value at `*pos`, advancing it past the value. When
/// `keep[index]` is `false` the value is skipped without being built (a column
/// the query never reads becomes NULL, and its LOB is not resolved).
fn decode_value(
    data: &[u8],
    pos: &mut usize,
    index: usize,
    lobs: Option<&dyn LobResolver>,
    keep: Option<&[bool]>,
) -> Result<Value> {
    let tag = take(data, pos, 1)?[0];
    let needed = match keep {
        None => true,
        Some(keep) => keep.get(index).copied().unwrap_or(true),
    };
    if !needed {
        skip_payload(data, pos, tag)?;
        return Ok(Value::Null);
    }
    Ok(match tag {
        TAG_NULL => Value::Null,
        TAG_INT => {
            let b = take(data, pos, 8)?;
            Value::Int(i64::from_le_bytes(b.try_into().unwrap()))
        }
        TAG_FLOAT => {
            let b = take(data, pos, 8)?;
            Value::Float(f64::from_le_bytes(b.try_into().unwrap()))
        }
        TAG_STR => {
            let lb = take(data, pos, 2)?;
            let len = u16::from_le_bytes(lb.try_into().unwrap()) as usize;
            let bytes = take(data, pos, len)?;
            Value::Str(
                String::from_utf8(bytes.to_vec())
                    .map_err(|_| Error::Runtime("invalid utf8 in stored string".into()))?,
            )
        }
        TAG_LOB => {
            let b = take(data, pos, 8)?;
            let id = u64::from_le_bytes(b.try_into().unwrap());
            let Some(lobs) = lobs else {
                return Err(Error::Runtime("lob reference without a resolver".into()));
            };
            let bytes = lobs.get(id)?;
            Value::Str(
                String::from_utf8(bytes)
                    .map_err(|_| Error::Runtime("invalid utf8 in stored lob".into()))?,
            )
        }
        TAG_BOOL => {
            let b = take(data, pos, 1)?;
            Value::Bool(b[0] != 0)
        }
        TAG_DATE => {
            let b = take(data, pos, 4)?;
            Value::Date(i32::from_le_bytes(b.try_into().unwrap()))
        }
        _ => return Err(Error::Runtime(format!("unknown value tag 0x{tag:02x}"))),
    })
}

fn take<'a>(data: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    if *pos + n > data.len() {
        return Err(Error::Runtime("truncated row data".into()));
    }
    let s = &data[*pos..*pos + n];
    *pos += n;
    Ok(s)
}

/// Advances `*pos` past the payload of an already-read `tag`, without building
/// a value. Shared by the skip path and the single-column decoder.
fn skip_payload(data: &[u8], pos: &mut usize, tag: u8) -> Result<()> {
    match tag {
        TAG_NULL => {}
        TAG_INT | TAG_FLOAT | TAG_LOB => {
            take(data, pos, 8)?;
        }
        TAG_BOOL => {
            take(data, pos, 1)?;
        }
        TAG_DATE => {
            take(data, pos, 4)?;
        }
        TAG_STR => {
            let lb = take(data, pos, 2)?;
            let len = u16::from_le_bytes(lb.try_into().unwrap()) as usize;
            take(data, pos, len)?;
        }
        _ => return Err(Error::Runtime(format!("unknown value tag 0x{tag:02x}"))),
    }
    Ok(())
}

/// Decodes only column `index` of a row encoding, skipping the other columns
/// without building them. Used by single-column scans.
pub(crate) fn decode_column(
    data: &[u8],
    index: usize,
    lobs: &dyn LobResolver,
) -> Result<Value> {
    let mut pos = 0;
    let hb = take(data, &mut pos, 2)?;
    let count = u16::from_le_bytes(hb.try_into().unwrap()) as usize;
    if index >= count {
        return Err(Error::Runtime("column index out of range".into()));
    }
    let mut target = Value::Null;
    for i in 0..count {
        if i == index {
            target = decode_value(data, &mut pos, i, Some(lobs), None)?;
        } else {
            let tag = take(data, &mut pos, 1)?[0];
            skip_payload(data, &mut pos, tag)?;
        }
    }
    Ok(target)
}

/// Decodes a single tagged value that stands alone (no row count header), as
/// stored in a PAX column segment.
pub(crate) fn decode_tagged_value(data: &[u8], lobs: &dyn LobResolver) -> Result<Value> {
    let mut pos = 0;
    decode_value(data, &mut pos, 0, Some(lobs), None)
}
