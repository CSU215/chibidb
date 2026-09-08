use crate::value::Value;
use crate::{Error, Result};

const TAG_NULL: u8 = 0x00;
const TAG_INT: u8 = 0x01;
const TAG_FLOAT: u8 = 0x02;
const TAG_STR: u8 = 0x03;
const TAG_BOOL: u8 = 0x04;

pub fn encode_row(row: &[Value]) -> Vec<u8> {
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
                buf.push(TAG_STR);
                buf.extend_from_slice(&(s.len() as u16).to_le_bytes());
                buf.extend_from_slice(s.as_bytes());
            }
            Value::Bool(b) => {
                buf.push(TAG_BOOL);
                buf.push(*b as u8);
            }
        }
    }
    buf
}

pub fn decode_row(data: &[u8]) -> Result<(Vec<Value>, usize)> {
    let mut pos = 0;
    let hb = take(data, &mut pos, 2)?;
    let count = u16::from_le_bytes(hb.try_into().unwrap()) as usize;
    let mut row = Vec::with_capacity(count);
    for _ in 0..count {
        let tag = data[pos];
        pos += 1;
        let v = match tag {
            TAG_NULL => Value::Null,
            TAG_INT => {
                let b = take(data, &mut pos, 8)?;
                Value::Int(i64::from_le_bytes(b.try_into().unwrap()))
            }
            TAG_FLOAT => {
                let b = take(data, &mut pos, 8)?;
                Value::Float(f64::from_le_bytes(b.try_into().unwrap()))
            }
            TAG_STR => {
                let lb = take(data, &mut pos, 2)?;
                let len = u16::from_le_bytes(lb.try_into().unwrap()) as usize;
                let bytes = take(data, &mut pos, len)?;
                Value::Str(String::from_utf8(bytes.to_vec()).map_err(|_| {
                    Error::Runtime("invalid utf8 in stored string".into())
                })?)
            }
            TAG_BOOL => {
                let b = take(data, &mut pos, 1)?;
                Value::Bool(b[0] != 0)
            }
            _ => return Err(Error::Runtime(format!("unknown value tag 0x{tag:02x}"))),
        };
        row.push(v);
    }
    Ok((row, pos))
}

fn take<'a>(data: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    if *pos + n > data.len() {
        return Err(Error::Runtime("truncated row data".into()));
    }
    let s = &data[*pos..*pos + n];
    *pos += n;
    Ok(s)
}
