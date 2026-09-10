use chibidb::storage::engine::{HeapEngine, TableEngine};
use chibidb::storage::{BufferPool, DiskManager, HeapFile, Rid};

fn setup(dir: &tempfile::TempDir, name: &str) -> (BufferPool, u32) {
    let mut disk = DiskManager::new();
    let file = disk.create_file(&dir.path().join(name)).unwrap();
    (BufferPool::new(disk, 8), file)
}

fn drain(bp: &mut BufferPool, file: u32) -> Vec<String> {
    let engine = HeapEngine::new(file);
    let mut scanner = engine.scan(bp).unwrap();
    let mut rows = Vec::new();
    while let Some((_, rec)) = scanner.next(bp).unwrap() {
        rows.push(String::from_utf8(rec).unwrap());
    }
    rows
}

#[test]
fn heap_engine_scans_visible_records_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let (mut bp, f) = setup(&dir, "a.dbf");
    let heap = HeapFile::init(&mut bp, f).unwrap();

    let rids: Vec<Rid> = ["r0", "r1", "r2"]
        .iter()
        .map(|r| heap.insert(&mut bp, r.as_bytes()).unwrap())
        .collect();
    heap.delete(&mut bp, rids[1]).unwrap();

    assert_eq!(drain(&mut bp, f), ["r0", "r2"]);
}

#[test]
fn heap_engine_scans_across_pages() {
    let dir = tempfile::tempdir().unwrap();
    let (mut bp, f) = setup(&dir, "b.dbf");
    let heap = HeapFile::init(&mut bp, f).unwrap();
    for _ in 0..200 {
        heap.insert(&mut bp, &[b'x'; 100]).unwrap();
    }
    assert_eq!(drain(&mut bp, f).len(), 200);
}

#[test]
fn heap_engine_get_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let (mut bp, f) = setup(&dir, "c.dbf");
    let heap = HeapFile::init(&mut bp, f).unwrap();
    let rid = heap.insert(&mut bp, b"hello").unwrap();

    let engine = HeapEngine::new(f);
    assert_eq!(engine.get(&mut bp, rid).unwrap(), b"hello");
}

#[test]
fn heap_engine_get_missing_record_errors() {
    let dir = tempfile::tempdir().unwrap();
    let (mut bp, f) = setup(&dir, "d.dbf");
    let heap = HeapFile::init(&mut bp, f).unwrap();
    let rid = heap.insert(&mut bp, b"only").unwrap();
    heap.delete(&mut bp, rid).unwrap();

    let engine = HeapEngine::new(f);
    assert!(engine.get(&mut bp, rid).is_err());
}
