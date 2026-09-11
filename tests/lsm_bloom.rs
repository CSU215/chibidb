use chibidb::storage::lsm::{BloomBuilder, BloomFilter};

#[test]
fn present_keys_are_never_rejected() {
    let mut builder = BloomBuilder::new(10);
    for i in 0..1000 {
        builder.add(format!("key{i:05}").as_bytes());
    }
    let filter = BloomFilter::decode(builder.finish());
    assert!(!filter.is_empty());
    for i in 0..1000 {
        assert!(
            filter.maybe_contains(format!("key{i:05}").as_bytes()),
            "bloom must not reject a present key {i}"
        );
    }
}

#[test]
fn absent_keys_are_mostly_rejected() {
    let mut builder = BloomBuilder::new(10);
    for i in 0..1000 {
        builder.add(format!("key{i:05}").as_bytes());
    }
    let filter = BloomFilter::decode(builder.finish());

    let mut false_positives = 0;
    for i in 0..1000 {
        if filter.maybe_contains(format!("absent{i:05}").as_bytes()) {
            false_positives += 1;
        }
    }
    assert!(false_positives < 100, "false positive rate too high: {false_positives}/1000");
}

#[test]
fn empty_filter_is_conservative() {
    let builder = BloomBuilder::new(10);
    assert_eq!(builder.key_count(), 0);
    let filter = BloomFilter::decode(builder.finish());
    assert!(filter.is_empty());
    assert!(filter.maybe_contains(b"anything"));
}
