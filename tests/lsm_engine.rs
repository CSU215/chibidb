use chibidb::storage::codec::encode_record_inline;
use chibidb::storage::engine::{TableEngine, TableStorage};
use chibidb::storage::lsm::engine::LSM_FILE_ID;
use chibidb::storage::lsm::LsmEngine;
use chibidb::storage::heap::Rid;
use chibidb::storage::{BufferPool, DiskManager};
use chibidb::value::Value;

fn pool() -> BufferPool {
    BufferPool::new(DiskManager::new(), 8)
}

#[test]
fn lsm_engine_supports_the_mvcc_record_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let bp = pool();
    let engine = LsmEngine::open(dir.path(), 256).unwrap();
    assert_eq!(engine.file_id(), LSM_FILE_ID);

    let data = encode_record_inline(1, 0, &[Value::Int(42)]);
    let rid = engine.insert(&bp, &data).unwrap();
    assert_eq!(engine.get(&bp, rid).unwrap(), data);

    // the scan yields the raw versioned record
    let mut scanner = engine.scan(&bp).unwrap();
    assert_eq!(scanner.next(&bp).unwrap(), Some((rid, data.clone())));
    assert!(scanner.next(&bp).unwrap().is_none());

    // delete-mark reports the previous marker
    assert_eq!(engine.delete_mark(&bp, rid, 7).unwrap(), 0);
    assert_eq!(engine.delete_mark(&bp, rid, 8).unwrap(), 7);

    // physical delete removes it
    engine.delete(&bp, rid).unwrap();
    assert!(engine.get(&bp, rid).is_err());
    let mut scanner = engine.scan(&bp).unwrap();
    assert!(scanner.next(&bp).unwrap().is_none());
}

#[test]
fn lsm_engine_flushed_data_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let bp = pool();

    let mut first: Option<Rid> = None;
    {
        let engine = LsmEngine::open(dir.path(), 256).unwrap();
        for i in 0..100 {
            let data = encode_record_inline(1, 0, &[Value::Int(i)]);
            let rid = engine.insert(&bp, &data).unwrap();
            first.get_or_insert(rid);
        }
        engine.flush().unwrap();
    }
    let first = first.unwrap();

    let engine = LsmEngine::open(dir.path(), 256).unwrap();
    // the old rows are readable and a new row gets a fresh id
    let data = encode_record_inline(2, 0, &[Value::Int(999)]);
    let new_rid = engine.insert(&bp, &data).unwrap();
    assert_ne!(new_rid, first);
    assert_eq!(engine.get(&bp, first).unwrap(), encode_record_inline(1, 0, &[Value::Int(0)]));

    let mut scanner = engine.scan(&bp).unwrap();
    let mut count = 0;
    while scanner.next(&bp).unwrap().is_some() {
        count += 1;
    }
    assert_eq!(count, 101);
}

#[test]
fn lsm_auto_compaction_bounds_the_table_count() {
    let dir = tempfile::tempdir().unwrap();
    let bp = pool();
    let engine = LsmEngine::open_with_trigger(dir.path(), 128, 3).unwrap();

    for round in 0..6u32 {
        for i in 0..10 {
            let data = encode_record_inline(round, 0, &[Value::Int(i)]);
            engine.insert(&bp, &data).unwrap();
        }
        engine.flush().unwrap();
        // with levels, the live count is bounded by about (trigger-1)*log(N),
        // well below the number of flushes
        assert!(
            engine.num_sstables() <= 4,
            "live tables {} exceeded the leveled bound",
            engine.num_sstables()
        );
    }

    let mut scanner = engine.scan(&bp).unwrap();
    let mut count = 0;
    while scanner.next(&bp).unwrap().is_some() {
        count += 1;
    }
    assert_eq!(count, 60);
}

#[test]
fn lsm_engine_compaction_preserves_values() {
    let dir = tempfile::tempdir().unwrap();
    let bp = pool();
    // a high trigger keeps auto-compaction out of the way of this test
    let engine = LsmEngine::open_with_trigger(dir.path(), 128, 100).unwrap();

    let mut rows = Vec::new();
    for round in 0..4u32 {
        for i in 0..20 {
            let data = encode_record_inline(round, 0, &[Value::Int(i)]);
            let rid = engine.insert(&bp, &data).unwrap();
            rows.push((rid, round, i));
        }
        engine.flush().unwrap();
    }
    assert_eq!(engine.num_sstables(), 4);
    engine.compact().unwrap();
    assert_eq!(engine.num_sstables(), 1);

    for (rid, round, i) in rows {
        assert_eq!(engine.get(&bp, rid).unwrap(), encode_record_inline(round, 0, &[Value::Int(i)]));
    }
}
