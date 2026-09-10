use chibidb::index::encode_key;
use chibidb::value::Value;

fn key(v: Value) -> Vec<u8> {
    encode_key(&v).unwrap()
}

#[test]
fn ints_order_preserving() {
    let keys: Vec<Vec<u8>> = [
        Value::Int(i64::MIN),
        Value::Int(-2),
        Value::Int(-1),
        Value::Int(0),
        Value::Int(1),
        Value::Int(2),
        Value::Int(i64::MAX),
    ]
    .iter()
    .map(|v| key(v.clone()))
    .collect();
    for w in keys.windows(2) {
        assert!(w[0] < w[1], "{:?} < {:?}", w[0], w[1]);
    }
    // adjacent ints must differ
    assert_ne!(key(Value::Int(0)), key(Value::Int(1)));
}

#[test]
fn floats_order_preserving() {
    let vals = [
        Value::Float(-f64::MAX),
        Value::Float(-2.5),
        Value::Float(-1.0),
        Value::Float(0.0),
        Value::Float(0.5),
        Value::Float(1.0),
        Value::Float(f64::MAX),
    ];
    let keys: Vec<Vec<u8>> = vals.iter().map(|v| key(v.clone())).collect();
    for w in keys.windows(2) {
        assert!(w[0] < w[1], "{:?} < {:?}", w[0], w[1]);
    }
    // -0.0 and 0.0 are numerically equal: identical encoding
    assert_eq!(key(Value::Float(-0.0)), key(Value::Float(0.0)));
}

#[test]
fn strings_order_preserving() {
    let expect: Vec<Vec<u8>> = ["", "a", "ab", "alice", "b"]
        .iter()
        .map(|s| key(Value::Str(s.to_string())))
        .collect();
    for w in expect.windows(2) {
        assert!(w[0] < w[1], "{:?} < {:?}", w[0], w[1]);
    }
}

#[test]
fn dates_and_nulls_order_preserving() {
    let null = key(Value::Null);
    let d1 = key(Value::Date(0));
    let d2 = key(Value::Date(10757));
    let d3 = key(Value::Date(i32::MAX));
    assert!(null < d1 && d1 < d2 && d2 < d3);
    assert!(key(Value::Date(-1)) < key(Value::Date(0)));
}

#[test]
fn booleans_are_not_indexable() {
    assert!(encode_key(&Value::Bool(true)).is_err());
}
