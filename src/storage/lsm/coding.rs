//! Variable- and fixed-width integer coding for LSM on-disk structures.
//!
//! Varints are LEB128: seven payload bits per byte, high bit set while more
//! bytes follow. Fixed-width integers are little-endian.

use crate::{Error, Result};

pub fn put_varint32(buf: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

pub fn put_varint64(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

/// Reads a varint32, advancing `pos`.
pub fn get_varint32(data: &[u8], pos: &mut usize) -> Result<u32> {
    let mut result: u32 = 0;
    for shift in (0..35).step_by(7) {
        let byte = take_byte(data, pos)?;
        result |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
    }
    Err(Error::Runtime("varint32 overflows 32 bits".into()))
}

/// Reads a varint64, advancing `pos`.
pub fn get_varint64(data: &[u8], pos: &mut usize) -> Result<u64> {
    let mut result: u64 = 0;
    for shift in (0..70).step_by(7) {
        let byte = take_byte(data, pos)?;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
    }
    Err(Error::Runtime("varint64 overflows 64 bits".into()))
}

pub fn put_fixed32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

pub fn get_fixed32(data: &[u8], pos: &mut usize) -> Result<u32> {
    let b = take(data, pos, 4)?;
    Ok(u32::from_le_bytes(b.try_into().unwrap()))
}

fn take_byte(data: &[u8], pos: &mut usize) -> Result<u8> {
    let b = *data
        .get(*pos)
        .ok_or_else(|| Error::Runtime("truncated varint".into()))?;
    *pos += 1;
    Ok(b)
}

fn take<'a>(data: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    if *pos + n > data.len() {
        return Err(Error::Runtime("truncated block data".into()));
    }
    let s = &data[*pos..*pos + n];
    *pos += n;
    Ok(s)
}
