use chaoticdb::storage::{BufferPool, DiskManager, HeapFile, PAGE_SIZE};

fn setup(dir: &tempfile::TempDir, name: &str) -> (BufferPool, u32) {
    let disk = DiskManager::new();
    disk.create_file(0, &dir.path().join(name)).unwrap();
    (BufferPool::new(disk, 8), 0)
}

#[test]
fn insert_and_get_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "a.dbf");
    let heap = HeapFile::init(&bp, f).unwrap();

    let r0 = heap.insert(&bp, b"hello").unwrap();
    let r1 = heap.insert(&bp, b"world!").unwrap();
    assert_eq!(r0.page_no, 1);
    assert_eq!(r0.slot, 0);
    assert_eq!(r1.slot, 1);
    assert_eq!(heap.get(&bp, r0).unwrap(), b"hello");
    assert_eq!(heap.get(&bp, r1).unwrap(), b"world!");
}

#[test]
fn records_spill_across_pages() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "b.dbf");
    let heap = HeapFile::init(&bp, f).unwrap();

    let n = 200;
    let mut rids = Vec::new();
    for i in 0..n {
        let mut rec = format!("record-{i:04}").into_bytes();
        rec.resize(100, b'.');
        rids.push(heap.insert(&bp, &rec).unwrap());
    }
    // ~78 records of 100 bytes per page: must use several pages
    let pages_used: std::collections::HashSet<_> = rids.iter().map(|r| r.page_no).collect();
    assert!(pages_used.len() >= 3, "pages used: {}", pages_used.len());

    for (i, rid) in rids.iter().enumerate() {
        let mut want = format!("record-{i:04}").into_bytes();
        want.resize(100, b'.');
        assert_eq!(heap.get(&bp, *rid).unwrap(), want);
    }
}

#[test]
fn delete_hides_record_from_scan() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "c.dbf");
    let heap = HeapFile::init(&bp, f).unwrap();

    let rids: Vec<_> = (0..5)
        .map(|i| heap.insert(&bp, format!("r{i}").as_bytes()).unwrap())
        .collect();
    heap.delete(&bp, rids[1]).unwrap();
    heap.delete(&bp, rids[3]).unwrap();

    let mut seen = Vec::new();
    heap.for_each(&bp, |rid, rec| {
        seen.push((rid, rec.to_vec()));
        Ok(())
    })
    .unwrap();
    let texts: Vec<String> = seen.iter().map(|(_, r)| String::from_utf8(r.clone()).unwrap()).collect();
    assert_eq!(texts, ["r0", "r2", "r4"]);
    assert!(heap.get(&bp, rids[1]).is_err());
}

#[test]
fn data_survives_pool_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.dbf");

    {
        let disk = DiskManager::new();
        disk.create_file(0, &path).unwrap();
        let bp = BufferPool::new(disk, 4);
        let heap = HeapFile::init(&bp, 0).unwrap();
        heap.insert(&bp, b"persist me").unwrap();
    }

    let disk = DiskManager::new();
    disk.open_file(0, &path).unwrap();
    let bp = BufferPool::new(disk, 4);
    let heap = HeapFile::open(&bp, 0).unwrap();
    let mut seen = Vec::new();
    heap.for_each(&bp, |_, rec| {
        seen.push(rec.to_vec());
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, [b"persist me".to_vec()]);
}

#[test]
fn replay_overwrites_a_stale_record_at_a_reused_rid() {
    // A rid can be freed without that free reaching the log (a VACUUM purge, or
    // a rollback). A later insert may reuse it, and on recovery the page is
    // read back from disk, which can still hold the stale record. `insert_at`
    // must overwrite rather than skip on mere occupancy.
    use chaoticdb::config::PageLayout;
    use chaoticdb::storage::engine::{HeapEngine, TableEngine, TableStorage};

    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "h.dbf");
    HeapFile::init(&bp, f).unwrap();
    let engine = HeapEngine::with_layout(f, PageLayout::Row);

    let rid = engine.insert(&bp, b"stale").unwrap();
    bp.flush_all().unwrap(); // the stale row reaches the disk
    engine.delete(&bp, rid).unwrap(); // rollback/vacuum frees the rid in memory
    bp.discard_file(f); // ...but that free never reaches the disk

    engine.insert_at(&bp, rid, b"fresh").unwrap();
    assert_eq!(engine.get(&bp, rid).unwrap(), b"fresh");
}

#[test]
fn open_rejects_foreign_files() {
    let dir = tempfile::tempdir().unwrap();

    let disk = DiskManager::new();
    disk.create_file(0, &dir.path().join("empty.dbf")).unwrap();
    let (bp, f) = (BufferPool::new(disk, 4), 0);
    assert!(HeapFile::open(&bp, f).is_err(), "empty file has no header");

    let disk2 = DiskManager::new();
    disk2.create_file(0, &dir.path().join("garbage.dbf")).unwrap();
    let (bp2, f) = (BufferPool::new(disk2, 4), 0);
    bp2.alloc_page(f).unwrap();
    bp2.with_page(f, 0, |p| {
        p[0..4].copy_from_slice(b"NOPE");
        Ok(())
    })
    .unwrap();
    assert!(HeapFile::open(&bp2, f).is_err());
}

#[test]
fn init_rejects_non_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "e.dbf");
    let _ = HeapFile::init(&bp, f).unwrap();
    assert!(HeapFile::init(&bp, f).is_err());
}

#[test]
fn max_record_fits_page() {
    let dir = tempfile::tempdir().unwrap();
    let (bp, f) = setup(&dir, "g.dbf");
    let heap = HeapFile::init(&bp, f).unwrap();
    let big = vec![b'x'; PAGE_SIZE / 2];
    let rid = heap.insert(&bp, &big).unwrap();
    assert_eq!(heap.get(&bp, rid).unwrap(), big);
}
