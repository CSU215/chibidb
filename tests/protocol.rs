use chaoticdb::protocol::{Protocol, TextProtocol};
use chaoticdb::value::Value;
use chaoticdb::wire::{decode_frame, Frame};
use chaoticdb::ResultSet;

fn request(sql: &str) -> Vec<u8> {
    let mut buf = (sql.len() as u32).to_le_bytes().to_vec();
    buf.extend_from_slice(sql.as_bytes());
    buf
}

#[test]
fn decodes_length_prefixed_request() {
    let mut p = TextProtocol;
    let bytes = request("select 1");
    assert_eq!(
        p.decode_request(&bytes).unwrap(),
        Some(("select 1".to_string(), bytes.len()))
    );
}

#[test]
fn waits_for_complete_request() {
    let mut p = TextProtocol;
    let bytes = request("select 1");
    assert_eq!(p.decode_request(&bytes[..2]).unwrap(), None);
    assert_eq!(p.decode_request(&bytes[..bytes.len() - 1]).unwrap(), None);
}

#[test]
fn decodes_one_request_at_a_time() {
    let mut p = TextProtocol;
    let mut bytes = request("select 1");
    let first_len = bytes.len();
    bytes.extend_from_slice(&request("select 2"));

    let (first, used) = p.decode_request(&bytes).unwrap().unwrap();
    assert_eq!(first, "select 1");
    assert_eq!(used, first_len);

    let (second, _) = p.decode_request(&bytes[used..]).unwrap().unwrap();
    assert_eq!(second, "select 2");
}

#[test]
fn encodes_results_then_done() {
    let p = TextProtocol;
    let results = vec![ResultSet::Rows {
        columns: vec!["x".to_string()],
        rows: vec![vec![Value::Int(1)]],
    }];
    let mut out = Vec::new();
    p.encode_success(&results, &mut out);

    let frames = split_frames(&out);
    assert_eq!(frames.len(), 2);
    match &frames[0] {
        Frame::Rows(ResultSet::Rows { rows, .. }) => assert_eq!(rows[0][0], Value::Int(1)),
        _ => panic!("expected a rows frame"),
    }
    assert!(matches!(&frames[1], Frame::Done));
}

#[test]
fn encodes_error_then_done() {
    let p = TextProtocol;
    let mut out = Vec::new();
    p.encode_failure("boom", &mut out);

    let frames = split_frames(&out);
    assert_eq!(frames.len(), 2);
    assert!(matches!(&frames[0], Frame::Error(m) if m == "boom"));
    assert!(matches!(&frames[1], Frame::Done));
}

fn split_frames(mut data: &[u8]) -> Vec<Frame> {
    let mut frames = Vec::new();
    while !data.is_empty() {
        let len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
        frames.push(decode_frame(&data[4..4 + len]).unwrap());
        data = &data[4 + len..];
    }
    frames
}
