use chibidb::storage::engine::{HeapEngine, TableEngine};
use chibidb::storage::{BufferPool, DiskManager, HeapFile, Rid};

fn setup(dir: &tempfile::TempDir, name: &str) -> (BufferPool, u32) {
    let mut disk = DiskManager::new();
    let file = disk.create_file(&dir.path().join(name)).unwrap();
    (BufferPool::new(disk, 8), file)
}

fn drain(bp: &BufferPool, file: u32) -> Vec<String> {
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
    let (bp, f) = setup(&dir, "a.dbf");
    let heap = HeapFile::init(&bp, f).unwrap();

    let rids: Vec<Rid> = ["r0", "r1", "r2"]
        .iter()
        .map(|r| heap.insert(&bp, r.as_bytes()).unwrap())
        .collect();
    heap.delete(&bp, rids[1]).unwrap();

    assert_eq!(drain(&bp, f), ["r0", "r2"]);
}

#[test]
fn heap_engine_scans_across_pages() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "b.dbf");
    let heap = HeapFile::init(&bp, f).unwrap();
    for _ in 0..200 {
        heap.insert(&bp, &[b'x'; 100]).unwrap();
    }
    assert_eq!(drain(&bp, f).len(), 200);
}

#[test]
fn heap_engine_get_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "c.dbf");
    let heap = HeapFile::init(&bp, f).unwrap();
    let rid = heap.insert(&bp, b"hello").unwrap();

    let engine = HeapEngine::new(f);
    assert_eq!(engine.get(&bp, rid).unwrap(), b"hello");
}

#[test]
fn heap_engine_get_missing_record_errors() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "d.dbf");
    let heap = HeapFile::init(&bp, f).unwrap();
    let rid = heap.insert(&bp, b"only").unwrap();
    heap.delete(&bp, rid).unwrap();

    let engine = HeapEngine::new(f);
    assert!(engine.get(&bp, rid).is_err());
}
