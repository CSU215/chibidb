use chibidb::index::{BTree, Bound};
use chibidb::storage::{BufferPool, DiskManager, Rid};

fn setup(dir: &tempfile::TempDir, name: &str) -> (BufferPool, u32) {
    let mut disk = DiskManager::new();
    let file = disk.create_file(&dir.path().join(name)).unwrap();
    (BufferPool::new(disk, 32), file)
}

#[test]
fn insert_and_search_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let (mut bp, f) = setup(&dir, "a.idxf");
    let tree = BTree::init(&mut bp, f).unwrap();

    tree.insert(&mut bp, b"a", Rid::new(1, 0)).unwrap();
    tree.insert(&mut bp, b"b", Rid::new(1, 1)).unwrap();
    tree.insert(&mut bp, b"c", Rid::new(2, 0)).unwrap();

    assert_eq!(tree.search(&mut bp, b"a").unwrap(), [Rid::new(1, 0)]);
    assert_eq!(tree.search(&mut bp, b"b").unwrap(), [Rid::new(1, 1)]);
    assert_eq!(tree.search(&mut bp, b"c").unwrap(), [Rid::new(2, 0)]);
    assert_eq!(tree.search(&mut bp, b"d").unwrap(), []);
    assert_eq!(tree.search(&mut bp, b"").unwrap(), []);
}

#[test]
fn duplicate_keys_are_all_returned() {
    let dir = tempfile::tempdir().unwrap();
    let (mut bp, f) = setup(&dir, "b.idxf");
    let tree = BTree::init(&mut bp, f).unwrap();

    tree.insert(&mut bp, b"dup", Rid::new(3, 0)).unwrap();
    tree.insert(&mut bp, b"dup", Rid::new(1, 5)).unwrap();
    tree.insert(&mut bp, b"dup", Rid::new(2, 9)).unwrap();

    let mut got = tree.search(&mut bp, b"dup").unwrap();
    got.sort();
    assert_eq!(got, [Rid::new(1, 5), Rid::new(2, 9), Rid::new(3, 0)]);
    assert_eq!(tree.search(&mut bp, b"other").unwrap(), []);
}

#[test]
fn survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.idxf");
    {
        let mut disk = DiskManager::new();
        let f = disk.create_file(&path).unwrap();
        let mut bp = BufferPool::new(disk, 8);
        let tree = BTree::init(&mut bp, f).unwrap();
        tree.insert(&mut bp, b"k1", Rid::new(7, 3)).unwrap();
        tree.insert(&mut bp, b"k2", Rid::new(7, 4)).unwrap();
    }
    let mut disk = DiskManager::new();
    let f = disk.open_file(&path).unwrap();
    let mut bp = BufferPool::new(disk, 8);
    let tree = BTree::open(&mut bp, f).unwrap();
    assert_eq!(tree.search(&mut bp, b"k1").unwrap(), [Rid::new(7, 3)]);
    assert_eq!(tree.search(&mut bp, b"k2").unwrap(), [Rid::new(7, 4)]);
}

#[test]
fn splits_grow_height_and_stay_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let (mut bp, f) = setup(&dir, "d.idxf");
    let tree = BTree::init(&mut bp, f).unwrap();

    let n = 500;
    for i in 0..n {
        let key = format!("key{i:06}");
        tree.insert(&mut bp, key.as_bytes(), Rid::new(1, i as u16)).unwrap();
    }
    assert!(tree.height(&mut bp).unwrap() >= 2, "tree must grow beyond a single leaf");
    for i in 0..n {
        let key = format!("key{i:06}");
        assert_eq!(tree.search(&mut bp, key.as_bytes()).unwrap(), [Rid::new(1, i as u16)], "{key}");
    }
    assert_eq!(tree.search(&mut bp, b"key000500").unwrap(), []);
}

#[test]
fn duplicates_survive_split_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let (mut bp, f) = setup(&dir, "e.idxf");
    let tree = BTree::init(&mut bp, f).unwrap();

    for i in 0..300 {
        tree.insert(&mut bp, b"dup", Rid::new(9, i as u16)).unwrap();
    }
    let got = tree.search(&mut bp, b"dup").unwrap();
    assert_eq!(got.len(), 300);
    for (i, rid) in got.iter().enumerate() {
        assert_eq!(rid.slot, i as u16);
    }
}

fn seeded_tree(dir: &tempfile::TempDir, name: &str, n: u32) -> (BufferPool, u32, BTree) {
    let (mut bp, f) = setup(dir, name);
    let tree = BTree::init(&mut bp, f).unwrap();
    for i in 0..n {
        let key = format!("key{i:06}");
        tree.insert(&mut bp, key.as_bytes(), Rid::new(1, i as u16)).unwrap();
    }
    (bp, f, tree)
}

fn keys_of(res: &[(Vec<u8>, Rid)]) -> Vec<String> {
    res.iter().map(|(k, _)| String::from_utf8(k.clone()).unwrap()).collect()
}

#[test]
fn scans_closed_range_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let (mut bp, _f, tree) = seeded_tree(&dir, "f.idxf", 500);

    let got = tree
        .scan_range(
            &mut bp,
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
    let (mut bp, _f, tree) = seeded_tree(&dir, "g.idxf", 500);

    let got = tree
        .scan_range(&mut bp, Bound::Included(b"key000100"), Bound::Excluded(b"key000200"))
        .unwrap();
    let keys = keys_of(&got);
    assert_eq!(keys.len(), 100);
    assert_eq!(keys[0], "key000100");
    assert_eq!(keys[99], "key000199");

    let got = tree
        .scan_range(&mut bp, Bound::Excluded(b"key000100"), Bound::Included(b"key000105"))
        .unwrap();
    let keys = keys_of(&got);
    assert_eq!(keys, ["key000101", "key000102", "key000103", "key000104", "key000105"]);

    let got = tree.scan_range(&mut bp, Bound::Unbounded, Bound::Unbounded).unwrap();
    let keys = keys_of(&got);
    assert_eq!(keys.len(), 500);
    assert_eq!(keys[0], "key000000");
    assert_eq!(keys[499], "key000499");
}

#[test]
fn scan_of_empty_and_missing_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let (mut bp, _f, tree) = seeded_tree(&dir, "h.idxf", 100);
    let got = tree
        .scan_range(&mut bp, Bound::Included(b"zzz"), Bound::Unbounded)
        .unwrap();
    assert_eq!(got.len(), 0);
    let got = tree
        .scan_range(&mut bp, Bound::Unbounded, Bound::Included(b"aaa"))
        .unwrap();
    assert_eq!(got.len(), 0);
}

#[test]
fn delete_removes_exact_entry() {
    let dir = tempfile::tempdir().unwrap();
    let (mut bp, _f, tree) = seeded_tree(&dir, "i.idxf", 500);

    tree.delete(&mut bp, b"key000250", Rid::new(1, 250)).unwrap();
    assert_eq!(tree.search(&mut bp, b"key000250").unwrap(), []);
    assert_eq!(tree.search(&mut bp, b"key000249").unwrap(), [Rid::new(1, 249)]);
    assert_eq!(tree.search(&mut bp, b"key000251").unwrap(), [Rid::new(1, 251)]);

    // deleting a non-existent entry errors
    assert!(tree.delete(&mut bp, b"key000250", Rid::new(1, 250)).is_err());
    assert!(tree.delete(&mut bp, b"missing", Rid::new(1, 1)).is_err());
}

#[test]
fn delete_duplicates_one_by_one() {
    let dir = tempfile::tempdir().unwrap();
    let (mut bp, f) = setup(&dir, "j.idxf");
    let tree = BTree::init(&mut bp, f).unwrap();
    for i in 0..5u16 {
        tree.insert(&mut bp, b"dup", Rid::new(3, i)).unwrap();
    }
    tree.delete(&mut bp, b"dup", Rid::new(3, 2)).unwrap();
    let mut got = tree.search(&mut bp, b"dup").unwrap();
    got.sort();
    assert_eq!(got, [Rid::new(3, 0), Rid::new(3, 1), Rid::new(3, 3), Rid::new(3, 4)]);
}

#[test]
fn deletes_cause_merge_and_height_shrink() {
    let dir = tempfile::tempdir().unwrap();
    let (mut bp, _f, tree) = seeded_tree(&dir, "k.idxf", 500);
    assert!(tree.height(&mut bp).unwrap() >= 2);

    // delete all but 20 keys
    for i in 20..500 {
        tree.delete(&mut bp, format!("key{i:06}").as_bytes(), Rid::new(1, i as u16))
            .unwrap();
    }
    assert_eq!(tree.height(&mut bp).unwrap(), 1, "tree must shrink back to one leaf");

    // remaining keys intact and ordered
    let got = tree.scan_range(&mut bp, Bound::Unbounded, Bound::Unbounded).unwrap();
    let keys = keys_of(&got);
    assert_eq!(keys.len(), 20);
    assert_eq!(keys[0], "key000000");
    assert_eq!(keys[19], "key000019");

    // delete the rest: tree becomes empty
    for i in 0..20 {
        tree.delete(&mut bp, format!("key{i:06}").as_bytes(), Rid::new(1, i as u16))
            .unwrap();
    }
    assert_eq!(tree.height(&mut bp).unwrap(), 0);
    assert_eq!(tree.search(&mut bp, b"key000000").unwrap(), []);
}

#[test]
fn delete_then_reinsert_stays_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let (mut bp, _f, tree) = seeded_tree(&dir, "l.idxf", 300);

    for i in 0..250 {
        tree.delete(&mut bp, format!("key{i:06}").as_bytes(), Rid::new(1, i as u16))
            .unwrap();
    }
    for i in 0..250 {
        let key = format!("key{i:06}");
        tree.insert(&mut bp, key.as_bytes(), Rid::new(2, i as u16)).unwrap();
    }
    let got = tree.scan_range(&mut bp, Bound::Unbounded, Bound::Unbounded).unwrap();
    assert_eq!(got.len(), 300);
    let keys = keys_of(&got);
    for w in keys.windows(2) {
        assert!(w[0] < w[1]);
    }
    assert_eq!(tree.search(&mut bp, b"key000100").unwrap(), [Rid::new(2, 100)]);
}

