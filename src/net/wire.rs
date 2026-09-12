use crate::result::ResultSet;
use crate::{Error, Result};

pub const KIND_MESSAGE: u8 = 0x00;
pub const KIND_ROWS: u8 = 0x01;
pub const KIND_ERROR: u8 = 0x02;
pub const KIND_DONE: u8 = 0x03;

/// Kind-prefixed standalone encoding of one ResultSet.
pub fn encode_result(rs: &ResultSet) -> Vec<u8> {
    match rs {
        ResultSet::Message(m) => {
            let mut buf = vec![KIND_MESSAGE];
            put_str(&mut buf, m);
            buf
        }
        ResultSet::Rows { columns, rows } => {
            let mut buf = vec![KIND_ROWS];
            put_rows_payload(&mut buf, columns, rows);
            buf
        }
    }
}

pub fn decode_result(data: &[u8]) -> Result<ResultSet> {
    let mut pos = 0;
    let kind = take(data, &mut pos, 1)?[0];
    match kind {
        KIND_MESSAGE => Ok(ResultSet::Message(take_str(data, &mut pos)?)),
        KIND_ROWS => decode_rows_payload(data, &mut pos),
        _ => Err(Error::Runtime(format!("unknown result kind 0x{kind:02x}"))),
    }
}

fn put_rows_payload(buf: &mut Vec<u8>, columns: &[String], rows: &[Vec<Value>]) {
    put_u32(buf, columns.len() as u32);
    for c in columns {
        put_str(buf, c);
    }
    put_u32(buf, rows.len() as u32);
    for row in rows {
        let encoded = crate::storage::codec::encode_row(row);
        put_u32(buf, encoded.len() as u32);
        buf.extend_from_slice(&encoded);
    }
}

fn decode_rows_payload(data: &[u8], pos: &mut usize) -> Result<ResultSet> {
    let ncols = take_u32(data, pos)? as usize;
    let mut columns = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        columns.push(take_str(data, pos)?);
    }
    let nrows = take_u32(data, pos)? as usize;
    let mut rows = Vec::with_capacity(nrows);
    for _ in 0..nrows {
        let len = take_u32(data, pos)? as usize;
        let bytes = take(data, pos, len)?;
        let (row, _) = crate::storage::codec::decode_row(bytes)?;
        rows.push(row);
    }
    Ok(ResultSet::Rows { columns, rows })
}

/// Frame body: [kind][payload].
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
        KIND_ROWS => Ok(Frame::Rows(decode_rows_payload(body, &mut pos)?)),
        _ => Err(Error::Runtime(format!("unknown frame kind 0x{kind:02x}"))),
    }
}

/// Encodes a ResultSet as a frame body whose kind matches the result type.
pub fn encode_result_frame(rs: &ResultSet) -> Vec<u8> {
    match rs {
        ResultSet::Message(m) => {
            let mut payload = Vec::new();
            put_str(&mut payload, m);
            encode_frame(KIND_MESSAGE, &payload)
        }
        ResultSet::Rows { columns, rows } => {
            let mut payload = Vec::new();
            put_rows_payload(&mut payload, columns, rows);
            encode_frame(KIND_ROWS, &payload)
        }
    }
}

/// Encodes an error string as an error-kind frame body.
pub fn encode_error_frame(msg: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    put_str(&mut payload, msg);
    encode_frame(KIND_ERROR, &payload)
}

/// Encodes an empty done frame.
pub fn encode_done_frame() -> Vec<u8> {
    encode_frame(KIND_DONE, &[])
}

use crate::value::Value;

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
