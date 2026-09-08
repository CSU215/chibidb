use chibidb::storage::codec::{decode_row, encode_row};
use chibidb::value::Value;

fn roundtrip(row: &[Value]) {
    let bytes = encode_row(row);
    let (decoded, used) = decode_row(&bytes).unwrap();
    assert_eq!(decoded, row);
    assert_eq!(used, bytes.len());
}

#[test]
fn encodes_and_decodes_rows() {
    roundtrip(&[]);
    roundtrip(&[Value::Int(0)]);
    roundtrip(&[Value::Int(-1), Value::Int(9223372036854775807)]);
    roundtrip(&[Value::Float(0.0), Value::Float(-2.5), Value::Float(95.5)]);
    roundtrip(&[Value::Str("".into()), Value::Str("alice".into())]);
    roundtrip(&[Value::Str("数据".into()), Value::Int(42), Value::Float(1.5)]);
    roundtrip(&[Value::Bool(true), Value::Bool(false)]);
}

#[test]
fn encoding_is_stable() {
    let bytes = encode_row(&[Value::Int(1)]);
    assert_eq!(bytes, vec![0x01, 1, 0, 0, 0, 0, 0, 0, 0]);
    let bytes = encode_row(&[Value::Str("ab".into())]);
    assert_eq!(bytes, vec![0x03, 2, 0, b'a', b'b']);
}

#[test]
fn decode_reports_consumed_bytes() {
    let mut buf = encode_row(&[Value::Int(1), Value::Str("x".into())]);
    let n_first = buf.len();
    buf.extend(encode_row(&[Value::Float(2.5)]));

    let (first, used) = decode_row(&buf).unwrap();
    assert_eq!(first, [Value::Int(1), Value::Str("x".into())]);
    assert_eq!(used, n_first);

    let (second, used2) = decode_row(&buf[used..]).unwrap();
    assert_eq!(second, [Value::Float(2.5)]);
    assert_eq!(used2, buf.len() - n_first);
}

#[test]
fn rejects_corrupt_data() {
    assert!(decode_row(&[0x7f]).is_err(), "unknown tag");
    assert!(decode_row(&[0x01, 1, 2]).is_err(), "truncated int");
    assert!(decode_row(&[0x03, 5, 0, b'a']).is_err(), "truncated string");
    assert!(decode_row(&[]).is_err(), "empty input");
}
