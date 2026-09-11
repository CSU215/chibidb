use chibidb::storage::lsm::block::{Block, BlockBuilder};
use chibidb::storage::lsm::coding::{
    get_fixed32, get_varint32, put_fixed32, put_varint32,
};

fn build(restart_interval: usize, count: usize) -> Vec<u8> {
    let mut builder = BlockBuilder::new(restart_interval);
    for i in 0..count {
        let key = format!("key{i:04}");
        let value = format!("value-{i}");
        builder.add(key.as_bytes(), value.as_bytes());
    }
    builder.finish()
}

#[test]
fn block_roundtrips_entries_in_order_with_prefix_compression() {
    let data = build(4, 300);
    let block = Block::parse(data).unwrap();
    let entries = block.entries().unwrap();
    assert_eq!(entries.len(), 300);
    for (i, (key, value)) in entries.iter().enumerate() {
        assert_eq!(key, format!("key{i:04}").as_bytes());
        assert_eq!(value, format!("value-{i}").as_bytes());
    }
    assert_eq!(block.first_key().unwrap(), Some(b"key0000".to_vec()));
}

#[test]
fn block_get_finds_existing_and_rejects_missing() {
    let block = Block::parse(build(2, 100)).unwrap();
    assert_eq!(block.get(b"key0000").unwrap(), Some(b"value-0".to_vec()));
    assert_eq!(block.get(b"key0050").unwrap(), Some(b"value-50".to_vec()));
    assert_eq!(block.get(b"key0099").unwrap(), Some(b"value-99".to_vec()));

    // between two keys, before the first, and after the last
    assert_eq!(block.get(b"key0050x").unwrap(), None);
    assert_eq!(block.get(b"aaa").unwrap(), None);
    assert_eq!(block.get(b"zzz").unwrap(), None);
}

#[test]
fn block_with_interval_one_matches_full_keys() {
    let block = Block::parse(build(1, 50)).unwrap();
    let entries = block.entries().unwrap();
    assert_eq!(entries.len(), 50);
    assert_eq!(block.get(b"key0025").unwrap(), Some(b"value-25".to_vec()));
}

#[test]
fn empty_block_parses_to_nothing() {
    let mut builder = BlockBuilder::new(16);
    assert!(builder.is_empty());
    let block = Block::parse(builder.finish()).unwrap();
    assert!(block.is_empty());
    assert_eq!(block.first_key().unwrap(), None);
    assert_eq!(block.get(b"anything").unwrap(), None);
    assert!(block.entries().unwrap().is_empty());
}

#[test]
fn varint_and_fixed_coding_roundtrip() {
    let mut data = Vec::new();
    for value in [0u32, 1, 127, 128, 300, u32::MAX] {
        put_varint32(&mut data, value);
    }
    put_fixed32(&mut data, 0xDEAD_BEEF);

    let mut pos = 0;
    for value in [0u32, 1, 127, 128, 300, u32::MAX] {
        assert_eq!(get_varint32(&data, &mut pos).unwrap(), value);
    }
    assert_eq!(get_fixed32(&data, &mut pos).unwrap(), 0xDEAD_BEEF);
    assert_eq!(pos, data.len());
}

#[test]
fn coding_rejects_truncated_input() {
    let data = vec![0x80, 0x80]; // varint with no terminator
    let mut pos = 0;
    assert!(get_varint32(&data, &mut pos).is_err());
    let mut pos = 0;
    assert!(get_fixed32(&[1, 2], &mut pos).is_err());
}
