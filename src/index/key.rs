use crate::value::Value;
use crate::{Error, Result};

const TAG_NULL: u8 = 0x00;
const TAG_INT: u8 = 0x01;
const TAG_FLOAT: u8 = 0x02;
const TAG_STR: u8 = 0x03;
const TAG_DATE: u8 = 0x04;

/// Order-preserving byte encoding so B+ tree nodes can compare keys with memcmp.
pub fn encode_key(v: &Value) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    encode_key_into(&mut buf, v)?;
    Ok(buf)
}

/// Appends the order-preserving encoding of `v` to `buf`, so callers can reuse
/// a scratch buffer instead of allocating a `Vec` per key.
pub fn encode_key_into(buf: &mut Vec<u8>, v: &Value) -> Result<()> {
    match v {
        Value::Null => buf.push(TAG_NULL),
        Value::Int(n) => {
            buf.push(TAG_INT);
            // sign-flipped big-endian keeps ordering for negative values
            let flipped = (*n as u64) ^ (1 << 63);
            buf.extend_from_slice(&flipped.to_be_bytes());
        }
        Value::Float(x) => {
            buf.push(TAG_FLOAT);
            let x = if *x == 0.0 { 0.0 } else { *x };
            let bits = x.to_bits();
            let ordered = if bits >> 63 == 1 { !bits } else { bits ^ (1 << 63) };
            buf.extend_from_slice(&ordered.to_be_bytes());
        }
        Value::Str(s) => {
            buf.push(TAG_STR);
            buf.extend_from_slice(s.as_bytes());
            buf.push(0x00);
        }
        Value::Date(d) => {
            buf.push(TAG_DATE);
            // sign-flip so negative days sort before positive ones
            let flipped = (*d as u32) ^ (1 << 31);
            buf.extend_from_slice(&flipped.to_be_bytes());
        }
        Value::Bool(_) => {
            return Err(Error::Runtime("cannot index boolean values".into()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_into_appends_the_same_bytes() {
        let values = [
            Value::Null,
            Value::Int(-7),
            Value::Int(0),
            Value::Int(42),
            Value::Float(-0.0),
            Value::Float(1.5),
            Value::Str("hello".into()),
            Value::Date(31),
        ];
        for value in &values {
            let owned = encode_key(value).unwrap();
            let mut buf = vec![0xAB];
            encode_key_into(&mut buf, value).unwrap();
            assert_eq!(&buf[1..], owned.as_slice(), "mismatch for {value:?}");
        }
        assert!(encode_key_into(&mut Vec::new(), &Value::Bool(true)).is_err());
    }
}
