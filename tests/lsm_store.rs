use chibidb::storage::lsm::{LsmStore, MergeScanner};

fn small() -> LsmStore {
    LsmStore::new(128)
}

#[test]
fn memtable_put_get_and_delete() {
    let mut store = small();
    store.put(b"a".to_vec(), b"1".to_vec());
    store.put(b"b".to_vec(), b"2".to_vec());
    assert_eq!(store.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(store.get(b"b").unwrap(), Some(b"2".to_vec()));

    store.put(b"a".to_vec(), b"9".to_vec());
    assert_eq!(store.get(b"a").unwrap(), Some(b"9".to_vec()));

    store.delete(b"a".to_vec());
    assert_eq!(store.get(b"a").unwrap(), None);
    assert_eq!(store.get(b"missing").unwrap(), None);
}

#[test]
fn flush_persists_to_sstables_and_newer_wins() {
    let mut store = small();
    for i in 0..50 {
        store.put(format!("key{i:03}").into_bytes(), format!("v{i}").into_bytes());
    }
    store.flush().unwrap();
    assert_eq!(store.num_sstables(), 1);
    assert_eq!(store.get(b"key007").unwrap(), Some(b"v7".to_vec()));

    // a newer value in the memtable shadows the flushed one
    store.put(b"key007".to_vec(), b"new".to_vec());
    assert_eq!(store.get(b"key007").unwrap(), Some(b"new".to_vec()));
}

#[test]
fn later_flush_shadows_earlier_tables() {
    let mut store = small();
    store.put(b"k".to_vec(), b"old".to_vec());
    store.flush().unwrap();
    store.put(b"k".to_vec(), b"new".to_vec());
    store.flush().unwrap();
    assert_eq!(store.num_sstables(), 2);
    assert_eq!(store.get(b"k").unwrap(), Some(b"new".to_vec()));
}

#[test]
fn delete_survives_flush_and_compaction() {
    let mut store = small();
    store.put(b"k".to_vec(), b"v".to_vec());
    store.flush().unwrap();
    store.delete(b"k".to_vec());
    store.flush().unwrap();
    // the tombstone in the newer table shadows the value in the older one
    assert_eq!(store.get(b"k").unwrap(), None);

    store.compact().unwrap();
    assert_eq!(store.num_sstables(), 1);
    // compaction dropped the tombstone; the key stays gone
    assert_eq!(store.get(b"k").unwrap(), None);
}

#[test]
fn iter_merges_memtable_and_sstables_in_order() {
    let mut store = small();
    store.put(b"b".to_vec(), b"1".to_vec());
    store.put(b"d".to_vec(), b"1".to_vec());
    store.flush().unwrap();
    store.put(b"a".to_vec(), b"2".to_vec());
    store.put(b"c".to_vec(), b"2".to_vec());
    store.put(b"d".to_vec(), b"2".to_vec());

    let entries = store.iter().unwrap();
    let keys: Vec<String> = entries
        .iter()
        .map(|(k, _)| String::from_utf8(k.clone()).unwrap())
        .collect();
    assert_eq!(keys, ["a", "b", "c", "d"]);
    assert_eq!(store.get(b"d").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn streaming_merge_yields_newest_visible_values() {
    let mut store = small();
    store.put(b"a".to_vec(), b"1".to_vec());
    store.put(b"b".to_vec(), b"1".to_vec());
    store.flush().unwrap();
    // newer memtable overrides `a` and deletes `b`
    store.put(b"a".to_vec(), b"2".to_vec());
    store.delete(b"b".to_vec());
    store.put(b"c".to_vec(), b"2".to_vec());

    let (mem, mut sstables) = store.snapshot();
    sstables.reverse(); // newest first
    let mut scanner = MergeScanner::new(mem, sstables).unwrap();
    let mut rows = Vec::new();
    while let Some(row) = scanner.next_entry().unwrap() {
        rows.push(row);
    }
    assert_eq!(rows, [(b"a".to_vec(), b"2".to_vec()), (b"c".to_vec(), b"2".to_vec())]);
}

#[test]
fn compaction_preserves_latest_values() {
    let mut store = small();
    for round in 0..5 {
        for i in 0..20 {
            store.put(format!("key{i:03}").into_bytes(), format!("r{round}-{i}").into_bytes());
        }
        store.flush().unwrap();
    }
    assert_eq!(store.num_sstables(), 5);
    store.compact().unwrap();
    assert_eq!(store.num_sstables(), 1);

    for i in 0..20 {
        assert_eq!(
            store.get(format!("key{i:03}").as_bytes()).unwrap(),
            Some(format!("r4-{i}").into_bytes())
        );
    }
    assert_eq!(store.iter().unwrap().len(), 20);
}
