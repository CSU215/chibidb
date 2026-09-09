use crate::result::ResultSet;
use crate::value::Value;
use crate::{Error, Result};

pub const KIND_MESSAGE: u8 = 0x00;
pub const KIND_ROWS: u8 = 0x01;
pub const KIND_ERROR: u8 = 0x02;
pub const KIND_DONE: u8 = 0x03;

pub fn encode_result(rs: &ResultSet) -> Vec<u8> {
    let mut buf = Vec::new();
    match rs {
        ResultSet::Message(m) => {
            buf.push(KIND_MESSAGE);
            put_str(&mut buf, m);
        }
        ResultSet::Rows { columns, rows } => {
            buf.push(KIND_ROWS);
            put_u32(&mut buf, columns.len() as u32);
            for c in columns {
                put_str(&mut buf, c);
            }
            put_u32(&mut buf, rows.len() as u32);
            for row in rows {
                let encoded = crate::storage::codec::encode_row(row);
                put_u32(&mut buf, encoded.len() as u32);
                buf.extend_from_slice(&encoded);
            }
        }
    }
    buf
}

pub fn decode_result(data: &[u8]) -> Result<ResultSet> {
    let mut pos = 0;
    let kind = take(data, &mut pos, 1)?[0];
    match kind {
        KIND_MESSAGE => {
            let m = take_str(data, &mut pos)?;
            Ok(ResultSet::Message(m))
        }
        KIND_ROWS => {
            let ncols = take_u32(data, &mut pos)? as usize;
            let mut columns = Vec::with_capacity(ncols);
            for _ in 0..ncols {
                columns.push(take_str(data, &mut pos)?);
            }
            let nrows = take_u32(data, &mut pos)? as usize;
            let mut rows = Vec::with_capacity(nrows);
            for _ in 0..nrows {
                let len = take_u32(data, &mut pos)? as usize;
                let bytes = take(data, &mut pos, len)?;
                let (row, _) = crate::storage::codec::decode_row(bytes)?;
                rows.push(row);
            }
            Ok(ResultSet::Rows { columns, rows })
        }
        _ => Err(Error::Runtime(format!("unknown result kind 0x{kind:02x}"))),
    }
}

/// Wire frame for one statement result, including the error and done markers.
pub fn encode_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.extend_from_slice(&((payload.len() + 1) as u32).to_le_bytes());
    buf.push(kind);
    buf.extend_from_slice(payload);
    buf
}

pub enum Frame {
    Message(String),
    Rows(ResultSet),
    Error(String),
    Done,
}

/// Reads one frame body (after the u32 length prefix).
pub fn decode_frame(body: &[u8]) -> Result<Frame> {
    let mut pos = 0;
    let kind = take(body, &mut pos, 1)?[0];
    match kind {
        KIND_MESSAGE => Ok(Frame::Message(take_str(body, &mut pos)?)),
        KIND_ERROR => Ok(Frame::Error(take_str(body, &mut pos)?)),
        KIND_DONE => Ok(Frame::Done),
        KIND_ROWS => {
            let rs = decode_result(body)?;
            Ok(Frame::Rows(rs))
        }
        _ => Err(Error::Runtime(format!("unknown frame kind 0x{kind:02x}"))),
    }
}

/// Encodes a ResultSet as a rows-kind frame body.
pub fn encode_rows_frame(rs: &ResultSet) -> Vec<u8> {
    encode_frame(KIND_ROWS, &encode_result(rs))
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
        return Err(Error::Runtime("truncated wire data".into()));
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
        .map_err(|_| Error::Runtime("invalid utf8 in wire data".into()))
}
