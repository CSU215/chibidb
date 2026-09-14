use chaoticdb::sql::result::ResultSet;
use chaoticdb::value::Value;
use chaoticdb::wire::{decode_result, encode_result};

#[test]
fn message_roundtrip() {
    let rs = ResultSet::Message("SUCCESS".into());
    assert_eq!(decode_result(&encode_result(&rs)).unwrap(), rs);
}

#[test]
fn rows_roundtrip() {
    let rs = ResultSet::Rows {
        columns: vec!["id".into(), "name".into(), "score".into()],
        rows: vec![
            vec![Value::Int(1), Value::Str("alice".into()), Value::Float(95.5)],
            vec![Value::Int(2), Value::Str("数据".into()), Value::Null],
        ],
    };
    assert_eq!(decode_result(&encode_result(&rs)).unwrap(), rs);
}

#[test]
fn empty_rows_roundtrip() {
    let rs = ResultSet::Rows { columns: vec!["id".into()], rows: vec![] };
    assert_eq!(decode_result(&encode_result(&rs)).unwrap(), rs);
}

#[test]
fn decode_rejects_garbage() {
    assert!(decode_result(&[]).is_err());
    assert!(decode_result(&[0xFF]).is_err(), "unknown kind");
    assert!(decode_result(&[0x01, 0xFF, 0xFF]).is_err(), "truncated rows");
    assert!(decode_result(&[0x00, 0xFF, 0xFF]).is_err(), "truncated message");
}
