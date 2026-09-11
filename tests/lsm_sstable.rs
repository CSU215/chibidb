use chibidb::storage::lsm::{SSTable, SSTableBuilder};

fn build(block_size: usize, count: usize) -> Vec<u8> {
    let mut builder = SSTableBuilder::new(block_size, 4);
    for i in 0..count {
        let key = format!("key{i:04}");
        let value = format!("value-{i}");
        builder.add(key.as_bytes(), value.as_bytes());
    }
    builder.finish()
}

#[test]
fn sstable_roundtrips_across_many_blocks() {
    let table = SSTable::parse(build(64, 200)).unwrap();
    assert!(table.num_blocks() > 1, "small blocks must split the table");
    assert_eq!(table.first_key().unwrap(), Some(b"key0000".to_vec()));

    let entries = table.iter().unwrap();
    assert_eq!(entries.len(), 200);
    for (i, (key, value)) in entries.iter().enumerate() {
        assert_eq!(key, format!("key{i:04}").as_bytes());
        assert_eq!(value, format!("value-{i}").as_bytes());
    }
}

#[test]
fn sstable_get_hits_and_misses() {
    let table = SSTable::parse(build(128, 100)).unwrap();
    assert_eq!(table.get(b"key0000").unwrap(), Some(b"value-0".to_vec()));
    assert_eq!(table.get(b"key0050").unwrap(), Some(b"value-50".to_vec()));
    assert_eq!(table.get(b"key0099").unwrap(), Some(b"value-99".to_vec()));
    assert_eq!(table.get(b"key0050z").unwrap(), None);
    assert_eq!(table.get(b"aaa").unwrap(), None);
    assert_eq!(table.get(b"zzz").unwrap(), None);
}

#[test]
fn single_block_table_works() {
    let table = SSTable::parse(build(4096, 5)).unwrap();
    assert_eq!(table.num_blocks(), 1);
    assert_eq!(table.iter().unwrap().len(), 5);
    assert_eq!(table.get(b"key0003").unwrap(), Some(b"value-3".to_vec()));
}

#[test]
fn empty_table_is_readable() {
    let mut builder = SSTableBuilder::new(64, 4);
    let table = SSTable::parse(builder.finish()).unwrap();
    assert_eq!(table.num_blocks(), 0);
    assert_eq!(table.first_key().unwrap(), None);
    assert_eq!(table.get(b"anything").unwrap(), None);
    assert!(table.iter().unwrap().is_empty());
}

#[test]
fn sstable_bloom_filter_is_wired() {
    let table = SSTable::parse(build(128, 500)).unwrap();
    for i in 0..500 {
        assert!(
            table.bloom().maybe_contains(format!("key{i:04}").as_bytes()),
            "present key {i} must pass the bloom filter"
        );
    }
    let rejected = (0..500)
        .filter(|i| !table.bloom().maybe_contains(format!("nope{i:04}").as_bytes()))
        .count();
    assert!(rejected > 450, "bloom should reject most absent keys, rejected {rejected}/500");
}

#[test]
fn parse_rejects_corruption() {
    // bad magic
    let mut image = build(4096, 3);
    let n = image.len();
    image[n - 1] = b'X';
    assert!(SSTable::parse(image).is_err());

    // truncated footer
    assert!(SSTable::parse(vec![0u8; 8]).is_err());
}
