use chaoticdb::storage::codec::encode_record_inline;
use chaoticdb::storage::engine::{HeapEngine, TableEngine, TableStorage};
use chaoticdb::storage::{BufferPool, DiskManager, HeapFile, Rid};
use chaoticdb::value::Value;

fn setup(dir: &tempfile::TempDir, name: &str) -> (BufferPool, u32) {
    let disk = DiskManager::new();
    disk.create_file(0, &dir.path().join(name)).unwrap();
    (BufferPool::new(disk, 8), 0)
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

#[test]
fn scanner_can_drain_in_batches() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "f.dbf");
    let heap = HeapFile::init(&bp, f).unwrap();
    for i in 0..25u8 {
        heap.insert(&bp, &[i]).unwrap();
    }

    let engine = HeapEngine::new(f);
    let mut scanner = engine.scan(&bp).unwrap();
    let mut rows = Vec::new();
    loop {
        let batch = scanner.next_batch(&bp, 4).unwrap();
        assert!(batch.len() <= 4);
        if batch.is_empty() {
            break;
        }
        rows.extend(batch.into_iter().map(|(_, rec)| rec[0]));
    }
    assert_eq!(rows, (0..25u8).collect::<Vec<_>>());
}

#[test]
fn scanner_next_into_matches_next() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "g.dbf");
    let heap = HeapFile::init(&bp, f).unwrap();
    for i in 0..200u8 {
        heap.insert(&bp, &[i; 100]).unwrap();
    }

    let engine = HeapEngine::new(f);
    let mut owned = engine.scan(&bp).unwrap();
    let mut reused = engine.scan(&bp).unwrap();
    let mut buf = Vec::new();
    loop {
        let a = owned.next(&bp).unwrap();
        let b = reused.next_into(&bp, &mut buf).unwrap();
        match (a, b) {
            (None, None) => break,
            (Some((rid_a, rec_a)), Some(rid_b)) => {
                assert_eq!(rid_a, rid_b, "row ids diverged");
                assert_eq!(rec_a, buf, "record bytes diverged");
            }
            (a, b) => panic!("scanners diverged: {a:?} vs {b:?}"),
        }
    }
}

#[test]
fn hinted_insert_matches_full_scan_inserts() {
    let dir = tempfile::tempdir().unwrap();
    let (bp_hint, f_hint) = setup(&dir, "hint.dbf");
    HeapFile::init(&bp_hint, f_hint).unwrap();
    let engine = HeapEngine::new(f_hint);
    let (bp_scan, f_scan) = setup(&dir, "noscan.dbf");
    let heap = HeapFile::init(&bp_scan, f_scan).unwrap();

    for i in 0..2000u32 {
        let rec = format!("row{i:05}").into_bytes();
        let hinted = engine.insert(&bp_hint, &rec).unwrap();
        let scanned = heap.insert(&bp_scan, &rec).unwrap();
        assert_eq!(hinted, scanned, "rid diverged at row {i}");
    }
    let hinted = drain(&bp_hint, f_hint);
    assert_eq!(hinted, drain(&bp_scan, f_scan));
    assert_eq!(hinted.len(), 2000);
}

#[test]
fn hinted_insert_wraps_to_reuse_freed_space() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "reuse.dbf");
    HeapFile::init(&bp, f).unwrap();
    let engine = HeapEngine::new(f);

    let rids: Vec<Rid> = (0..600u32)
        .map(|i| engine.insert(&bp, &[b'a' + (i % 26) as u8; 60]).unwrap())
        .collect();
    // free half the records, leaving holes across the pages
    for (i, rid) in rids.iter().enumerate() {
        if i % 2 == 0 {
            engine.delete(&bp, *rid).unwrap();
        }
    }
    // leftovers plus fresh inserts must all be readable
    for _ in 0..300 {
        engine.insert(&bp, &[b'0'; 60]).unwrap();
    }
    let rows = drain(&bp, f);
    assert_eq!(rows.len(), 300 + 300);
    assert_eq!(rows.iter().filter(|r| r.starts_with('0')).count(), 300);
}

#[test]
fn table_storage_supports_the_mvcc_version_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "e.dbf");
    HeapFile::init(&bp, f).unwrap();
    let engine = HeapEngine::new(f);
    assert_eq!(engine.file_id(), f);

    // insert a version (creator, deleter, row) and read it back
    let data = encode_record_inline(1, 0, 0, &[Value::Int(42)]);
    let rid = engine.insert(&bp, &data).unwrap();
    assert_eq!(engine.get(&bp, rid).unwrap(), data);

    // a delete-mark reports the previous (zero) marker and pointer
    assert_eq!(engine.delete_mark(&bp, rid, 7, 0).unwrap(), (0, 0));
    // and a second marker reports the first
    assert_eq!(engine.delete_mark(&bp, rid, 8, 0).unwrap(), (7, 0));

    // physical delete removes it from the scan
    engine.delete(&bp, rid).unwrap();
    let mut scanner = engine.scan(&bp).unwrap();
    assert!(scanner.next(&bp).unwrap().is_none());
}
