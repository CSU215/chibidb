use std::sync::Arc;

use chaoticdb::storage::lsm::{MemEntry, MemTable};

#[test]
fn put_and_get_roundtrip_with_overwrite() {
    let mt = MemTable::new();
    mt.put(b"a".to_vec(), b"1".to_vec());
    mt.put(b"b".to_vec(), b"2".to_vec());
    mt.put(b"a".to_vec(), b"3".to_vec());

    assert_eq!(mt.get(b"a"), Some(MemEntry::Value(b"3".to_vec())));
    assert_eq!(mt.get(b"b"), Some(MemEntry::Value(b"2".to_vec())));
    assert_eq!(mt.get(b"missing"), None);
    assert_eq!(mt.len(), 2);
    assert!(!mt.is_empty());
}

#[test]
fn delete_writes_a_tombstone() {
    let mt = MemTable::new();
    mt.put(b"k".to_vec(), b"v".to_vec());
    mt.delete(b"k".to_vec());

    let entry = mt.get(b"k").unwrap();
    assert!(entry.is_tombstone());
    assert_eq!(entry.value(), None);
    // the key is still present so it can shadow older levels
    assert_eq!(mt.len(), 1);
}

#[test]
fn iteration_is_sorted_and_range_is_half_open() {
    let mt = MemTable::new();
    for key in ["d", "b", "a", "c"] {
        mt.put(key.as_bytes().to_vec(), key.as_bytes().to_vec());
    }
    let keys: Vec<String> = mt
        .iter()
        .into_iter()
        .map(|(k, _)| String::from_utf8(k).unwrap())
        .collect();
    assert_eq!(keys, ["a", "b", "c", "d"]);

    let keys: Vec<String> = mt
        .range(b"b", b"d")
        .into_iter()
        .map(|(k, _)| String::from_utf8(k).unwrap())
        .collect();
    assert_eq!(keys, ["b", "c"]);
}

#[test]
fn byte_accounting_tracks_overwrites_and_tombstones() {
    let mt = MemTable::new();
    mt.put(b"key".to_vec(), b"12345".to_vec());
    assert_eq!(mt.approx_bytes(), 3 + 5);

    // overwrite with a smaller value shrinks the count
    mt.put(b"key".to_vec(), b"1".to_vec());
    assert_eq!(mt.approx_bytes(), 3 + 1);

    // a tombstone keeps only the key bytes
    mt.delete(b"key".to_vec());
    assert_eq!(mt.approx_bytes(), 3);
}

#[test]
fn shared_memtable_accepts_concurrent_writes() {
    let mt = Arc::new(MemTable::new());
    let mut handles = Vec::new();
    for worker in 0..8u32 {
        let mt = Arc::clone(&mt);
        handles.push(std::thread::spawn(move || {
            for i in 0..100u32 {
                mt.put(
                    format!("{worker:02}-{i:03}").into_bytes(),
                    i.to_le_bytes().to_vec(),
                );
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(mt.len(), 800);
    for worker in 0..8u32 {
        for i in 0..100u32 {
            let key = format!("{worker:02}-{i:03}").into_bytes();
            assert_eq!(mt.get(&key), Some(MemEntry::Value(i.to_le_bytes().to_vec())));
        }
    }
}
