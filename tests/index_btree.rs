use chibidb::index::BTree;
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
