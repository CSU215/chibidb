use chaoticdb::index::{BTree, Bound};
use chaoticdb::storage::{BufferPool, DiskManager, Rid};

fn setup(dir: &tempfile::TempDir, name: &str) -> (BufferPool, u32) {
    let disk = DiskManager::new();
    disk.create_file(0, &dir.path().join(name)).unwrap();
    (BufferPool::new(disk, 32), 0)
}

#[test]
fn insert_and_search_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "a.idxf");
    let tree = BTree::init(&bp, f).unwrap();

    tree.insert(&bp, b"a", Rid::new(1, 0)).unwrap();
    tree.insert(&bp, b"b", Rid::new(1, 1)).unwrap();
    tree.insert(&bp, b"c", Rid::new(2, 0)).unwrap();

    assert_eq!(tree.search(&bp, b"a").unwrap(), [Rid::new(1, 0)]);
    assert_eq!(tree.search(&bp, b"b").unwrap(), [Rid::new(1, 1)]);
    assert_eq!(tree.search(&bp, b"c").unwrap(), [Rid::new(2, 0)]);
    assert_eq!(tree.search(&bp, b"d").unwrap(), []);
    assert_eq!(tree.search(&bp, b"").unwrap(), []);
}

#[test]
fn duplicate_keys_are_all_returned() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "b.idxf");
    let tree = BTree::init(&bp, f).unwrap();

    tree.insert(&bp, b"dup", Rid::new(3, 0)).unwrap();
    tree.insert(&bp, b"dup", Rid::new(1, 5)).unwrap();
    tree.insert(&bp, b"dup", Rid::new(2, 9)).unwrap();

    let mut got = tree.search(&bp, b"dup").unwrap();
    got.sort();
    assert_eq!(got, [Rid::new(1, 5), Rid::new(2, 9), Rid::new(3, 0)]);
    assert_eq!(tree.search(&bp, b"other").unwrap(), []);
}

#[test]
fn survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.idxf");
    {
        let disk = DiskManager::new();
        disk.create_file(0, &path).unwrap();
        let bp = BufferPool::new(disk, 8);
        let tree = BTree::init(&bp, 0).unwrap();
        tree.insert(&bp, b"k1", Rid::new(7, 3)).unwrap();
        tree.insert(&bp, b"k2", Rid::new(7, 4)).unwrap();
    }
    let disk = DiskManager::new();
    disk.open_file(0, &path).unwrap();
    let bp = BufferPool::new(disk, 8);
    let tree = BTree::open(&bp, 0).unwrap();
    assert_eq!(tree.search(&bp, b"k1").unwrap(), [Rid::new(7, 3)]);
    assert_eq!(tree.search(&bp, b"k2").unwrap(), [Rid::new(7, 4)]);
}

#[test]
fn splits_grow_height_and_stay_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "d.idxf");
    let tree = BTree::init(&bp, f).unwrap();

    let n = 500;
    for i in 0..n {
        let key = format!("key{i:06}");
        tree.insert(&bp, key.as_bytes(), Rid::new(1, i as u16)).unwrap();
    }
    assert!(tree.height(&bp).unwrap() >= 2, "tree must grow beyond a single leaf");
    for i in 0..n {
        let key = format!("key{i:06}");
        assert_eq!(tree.search(&bp, key.as_bytes()).unwrap(), [Rid::new(1, i as u16)], "{key}");
    }
    assert_eq!(tree.search(&bp, b"key000500").unwrap(), []);
}

#[test]
fn duplicates_survive_split_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "e.idxf");
    let tree = BTree::init(&bp, f).unwrap();

    for i in 0..300 {
        tree.insert(&bp, b"dup", Rid::new(9, i as u16)).unwrap();
    }
    let got = tree.search(&bp, b"dup").unwrap();
    assert_eq!(got.len(), 300);
    for (i, rid) in got.iter().enumerate() {
        assert_eq!(rid.slot, i as u16);
    }
}

fn seeded_tree(dir: &tempfile::TempDir, name: &str, n: u32) -> (BufferPool, u32, BTree) {
    let (bp, f) = setup(dir, name);
    let tree = BTree::init(&bp, f).unwrap();
    for i in 0..n {
        let key = format!("key{i:06}");
        tree.insert(&bp, key.as_bytes(), Rid::new(1, i as u16)).unwrap();
    }
    (bp, f, tree)
}

fn keys_of(res: &[(Vec<u8>, Rid)]) -> Vec<String> {
    res.iter().map(|(k, _)| String::from_utf8(k.clone()).unwrap()).collect()
}

#[test]
fn scans_closed_range_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, _f, tree) = seeded_tree(&dir, "f.idxf", 500);

    let got = tree
        .scan_range(
            &bp,
            Bound::Included(b"key000100"),
            Bound::Included(b"key000199"),
        )
        .unwrap();
    let keys = keys_of(&got);
    assert_eq!(keys.len(), 100);
    assert_eq!(keys[0], "key000100");
    assert_eq!(keys[99], "key000199");
    for w in keys.windows(2) {
        assert!(w[0] < w[1], "{} < {}", w[0], w[1]);
    }
    assert_eq!(got[0].1, Rid::new(1, 100));
}

#[test]
fn scans_open_and_unbounded_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, _f, tree) = seeded_tree(&dir, "g.idxf", 500);

    let got = tree
        .scan_range(&bp, Bound::Included(b"key000100"), Bound::Excluded(b"key000200"))
        .unwrap();
    let keys = keys_of(&got);
    assert_eq!(keys.len(), 100);
    assert_eq!(keys[0], "key000100");
    assert_eq!(keys[99], "key000199");

    let got = tree
        .scan_range(&bp, Bound::Excluded(b"key000100"), Bound::Included(b"key000105"))
        .unwrap();
    let keys = keys_of(&got);
    assert_eq!(keys, ["key000101", "key000102", "key000103", "key000104", "key000105"]);

    let got = tree.scan_range(&bp, Bound::Unbounded, Bound::Unbounded).unwrap();
    let keys = keys_of(&got);
    assert_eq!(keys.len(), 500);
    assert_eq!(keys[0], "key000000");
    assert_eq!(keys[499], "key000499");
}

#[test]
fn scan_of_empty_and_missing_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, _f, tree) = seeded_tree(&dir, "h.idxf", 100);
    let got = tree
        .scan_range(&bp, Bound::Included(b"zzz"), Bound::Unbounded)
        .unwrap();
    assert_eq!(got.len(), 0);
    let got = tree
        .scan_range(&bp, Bound::Unbounded, Bound::Included(b"aaa"))
        .unwrap();
    assert_eq!(got.len(), 0);
}

#[test]
fn delete_removes_exact_entry() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, _f, tree) = seeded_tree(&dir, "i.idxf", 500);

    tree.delete(&bp, b"key000250", Rid::new(1, 250)).unwrap();
    assert_eq!(tree.search(&bp, b"key000250").unwrap(), []);
    assert_eq!(tree.search(&bp, b"key000249").unwrap(), [Rid::new(1, 249)]);
    assert_eq!(tree.search(&bp, b"key000251").unwrap(), [Rid::new(1, 251)]);

    // deleting a non-existent entry errors
    assert!(tree.delete(&bp, b"key000250", Rid::new(1, 250)).is_err());
    assert!(tree.delete(&bp, b"missing", Rid::new(1, 1)).is_err());
}

#[test]
fn delete_duplicates_one_by_one() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "j.idxf");
    let tree = BTree::init(&bp, f).unwrap();
    for i in 0..5u16 {
        tree.insert(&bp, b"dup", Rid::new(3, i)).unwrap();
    }
    tree.delete(&bp, b"dup", Rid::new(3, 2)).unwrap();
    let mut got = tree.search(&bp, b"dup").unwrap();
    got.sort();
    assert_eq!(got, [Rid::new(3, 0), Rid::new(3, 1), Rid::new(3, 3), Rid::new(3, 4)]);
}

#[test]
fn deletes_cause_merge_and_height_shrink() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, _f, tree) = seeded_tree(&dir, "k.idxf", 500);
    assert!(tree.height(&bp).unwrap() >= 2);

    // delete all but 20 keys
    for i in 20..500 {
        tree.delete(&bp, format!("key{i:06}").as_bytes(), Rid::new(1, i as u16))
            .unwrap();
    }
    assert_eq!(tree.height(&bp).unwrap(), 1, "tree must shrink back to one leaf");

    // remaining keys intact and ordered
    let got = tree.scan_range(&bp, Bound::Unbounded, Bound::Unbounded).unwrap();
    let keys = keys_of(&got);
    assert_eq!(keys.len(), 20);
    assert_eq!(keys[0], "key000000");
    assert_eq!(keys[19], "key000019");

    // delete the rest: tree becomes empty
    for i in 0..20 {
        tree.delete(&bp, format!("key{i:06}").as_bytes(), Rid::new(1, i as u16))
            .unwrap();
    }
    assert_eq!(tree.height(&bp).unwrap(), 0);
    assert_eq!(tree.search(&bp, b"key000000").unwrap(), []);
}

#[test]
fn delete_then_reinsert_stays_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, _f, tree) = seeded_tree(&dir, "l.idxf", 300);

    for i in 0..250 {
        tree.delete(&bp, format!("key{i:06}").as_bytes(), Rid::new(1, i as u16))
            .unwrap();
    }
    for i in 0..250 {
        let key = format!("key{i:06}");
        tree.insert(&bp, key.as_bytes(), Rid::new(2, i as u16)).unwrap();
    }
    let got = tree.scan_range(&bp, Bound::Unbounded, Bound::Unbounded).unwrap();
    assert_eq!(got.len(), 300);
    let keys = keys_of(&got);
    for w in keys.windows(2) {
        assert!(w[0] < w[1]);
    }
    assert_eq!(tree.search(&bp, b"key000100").unwrap(), [Rid::new(2, 100)]);
}

#[test]
fn long_key_split_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "long.idxf");
    let tree = BTree::init(&bp, f).unwrap();

    // fill a leaf with short keys, then force a byte-aware split with a long key
    let n = 600u32;
    for i in 0..n {
        let k = format!("k{i:04}");
        tree.insert(&bp, k.as_bytes(), Rid::new(i / 8, (i % 8) as u16)).unwrap();
    }
    let long = vec![b'z'; 3000];
    tree.insert(&bp, &long, Rid::new(9, 9)).unwrap();

    for i in 0..n {
        let k = format!("k{i:04}");
        assert_eq!(tree.search(&bp, k.as_bytes()).unwrap(), [Rid::new(i / 8, (i % 8) as u16)]);
    }
    assert_eq!(tree.search(&bp, &long).unwrap(), [Rid::new(9, 9)]);
}

#[test]
fn duplicate_run_delete_across_split() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "dups.idxf");
    let tree = BTree::init(&bp, f).unwrap();

    let n = 1200u16;
    let key = b"dup";
    let rid = |i: u16| Rid::new((i / 16) as u32, i % 16);
    for i in 0..n {
        tree.insert(&bp, key, rid(i)).unwrap();
    }
    // delete the first, a middle and the last entry of the duplicate run
    for i in [0, n / 2, n - 1] {
        tree.delete(&bp, key, rid(i)).unwrap();
    }

    let mut got = tree.search(&bp, key).unwrap();
    got.sort();
    let mut want: Vec<Rid> =
        (0..n).filter(|i| *i != 0 && *i != n / 2 && *i != n - 1).map(rid).collect();
    want.sort();
    assert_eq!(got, want);
}


