use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chaoticdb::config::Config;
use chaoticdb::value::Value;
use chaoticdb::{Database, ResultSet, Session};

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute_sql(sql).unwrap().remove(0) {
        ResultSet::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn messages(db: &Database, sql: &str) -> String {
    match db.execute_sql(sql).unwrap().remove(0) {
        ResultSet::Message(m) => m,
        other => panic!("expected message, got {other:?}"),
    }
}

#[test]
fn committed_insert_survives_crash() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int, name char(10));").unwrap();
        db.execute_sql("insert into t values (1, 'alice'), (2, 'bob');").unwrap();
        // no flush: dirty pages stay in the buffer pool, only the WAL is durable
        db.simulate_crash();
    }
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(
        rows(&db, "select * from t order by id;"),
        vec![
            vec![Value::Int(1), Value::Str("alice".into())],
            vec![Value::Int(2), Value::Str("bob".into())],
        ]
    );
}

#[test]
fn failed_statement_leaves_no_redo_for_the_transaction() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        let mut s = Session::new();
        db.execute_sql_with(&mut s, "create table t (id int, name char(4));").unwrap();
        db.flush().unwrap();
        db.execute_sql_with(&mut s, "begin;").unwrap();
        db.execute_sql_with(&mut s, "insert into t values (1, 'ok');").unwrap();
        // the first row is stored before the second fails, so the failed
        // statement's buffered redo must be dropped, not replayed at commit
        assert!(
            db.execute_sql_with(&mut s, "insert into t values (2, 'ok'), (3, 'toolong');")
                .is_err()
        );
        db.execute_sql_with(&mut s, "commit;").unwrap();
        db.simulate_crash();
    }
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&db, "select id from t order by id;"), vec![vec![Value::Int(1)]]);
}

#[test]
fn committed_delete_survives_crash() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int);").unwrap();
        db.execute_sql("insert into t values (1), (2);").unwrap();
        db.flush().unwrap();
        db.execute_sql("delete from t where id = 1;").unwrap();
        db.simulate_crash();
    }
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&db, "select * from t order by id;"), vec![vec![Value::Int(2)]]);
}

#[test]
fn committed_update_survives_crash() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int, score float);").unwrap();
        db.execute_sql("insert into t values (1, 10.0), (2, 20.0);").unwrap();
        db.flush().unwrap();
        db.execute_sql("update t set score = score + 1 where id = 1;").unwrap();
        db.simulate_crash();
    }
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(
        rows(&db, "select * from t order by id;"),
        vec![
            vec![Value::Int(1), Value::Float(11.0)],
            vec![Value::Int(2), Value::Float(20.0)],
        ]
    );
}

#[test]
fn uncommitted_insert_does_not_reappear() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int, name char(10));").unwrap();
        db.execute_sql("begin; insert into t values (1, 'ghost');").unwrap();
        // crash with the transaction still open
        db.simulate_crash();
    }
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&db, "select * from t;"), Vec::<Vec<Value>>::new());

    // the aborted trx id must not be reused, otherwise the next transaction
    // would adopt the ghost row as its own
    db.execute_sql("insert into t values (2, 'real');").unwrap();
    assert_eq!(
        rows(&db, "select * from t order by id;"),
        vec![vec![Value::Int(2), Value::Str("real".into())]]
    );

    // and the recovered state must be stable across another crash/reopen
    drop(db);
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(
        rows(&db, "select * from t order by id;"),
        vec![vec![Value::Int(2), Value::Str("real".into())]]
    );
}

#[test]
fn committed_indexed_insert_survives_crash() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int, name char(10));").unwrap();
        db.execute_sql("create index idx_id on t (id);").unwrap();
        db.execute_sql("insert into t values (1, 'alice'), (2, 'bob');").unwrap();
        db.simulate_crash();
    }
    let db = Database::open(dir.path()).unwrap();
    let plan = messages(&db, "explain select * from t where id = 1;");
    assert!(plan.contains("IndexScan"), "plan: {plan}");
    assert_eq!(
        rows(&db, "select * from t where id = 2;"),
        vec![vec![Value::Int(2), Value::Str("bob".into())]]
    );
}

#[test]
fn truncated_tail_frame_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int);").unwrap();
        db.execute_sql("insert into t values (7);").unwrap();
        db.simulate_crash();
    }
    // tear the log mid-frame: a length header that promises more bytes than exist
    let wal_path = dir.path().join("wal.bin");
    let mut f = std::fs::OpenOptions::new().append(true).open(&wal_path).unwrap();
    use std::io::Write;
    f.write_all(&100u32.to_le_bytes()).unwrap();
    f.write_all(b"tw").unwrap();
    drop(f);

    let db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&db, "select * from t;"), vec![vec![Value::Int(7)]]);
}

#[test]
fn clean_flush_truncates_wal() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("wal.bin");
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int);").unwrap();
        db.execute_sql("insert into t values (1);").unwrap();
        assert!(std::fs::metadata(&wal_path).unwrap().len() > 0, "wal must hold redo records");
        db.flush().unwrap();
        assert_eq!(std::fs::metadata(&wal_path).unwrap().len(), 0, "clean flush is a checkpoint");
    }
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&db, "select * from t;"), vec![vec![Value::Int(1)]]);
}

#[test]
fn drop_table_survives_crash_with_stale_wal() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int);").unwrap();
        db.execute_sql("insert into t values (1);").unwrap();
        db.execute_sql("drop table t;").unwrap();
        // the log still holds records for the dropped table's file
        db.simulate_crash();
    }
    // recovery must skip stale records of the dropped table, not fail
    let db = Database::open(dir.path()).unwrap();
    let err = db.execute_sql("select * from t;").unwrap_err();
    assert!(err.to_string().contains("no such table"), "{err}");
    db.execute_sql("create table t (id int);").unwrap();
    db.execute_sql("insert into t values (5);").unwrap();
    assert_eq!(rows(&db, "select * from t;"), vec![vec![Value::Int(5)]]);
}

#[test]
fn checkpoint_statement_flushes_and_truncates_wal() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("wal.bin");
    let db = Database::open(dir.path()).unwrap();
    db.execute_sql("create table t (id int);").unwrap();
    db.execute_sql("insert into t values (1);").unwrap();
    assert!(std::fs::metadata(&wal_path).unwrap().len() > 0, "precondition: wal has records");

    db.execute_sql("checkpoint;").unwrap();

    assert_eq!(std::fs::metadata(&wal_path).unwrap().len(), 0, "checkpoint clears the log");
    assert_eq!(rows(&db, "select * from t;"), vec![vec![Value::Int(1)]]);
    drop(db);
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&db, "select * from t;"), vec![vec![Value::Int(1)]]);
}

#[test]
fn checkpoint_rejects_open_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    db.execute_sql("create table t (id int);").unwrap();
    let mut session = chaoticdb::Session::new();
    db.execute_sql_with(&mut session, "begin;").unwrap();
    let err = db.execute_sql_with(&mut session, "checkpoint;").unwrap_err();
    assert!(err.to_string().contains("transaction"), "{err}");
    db.execute_sql_with(&mut session, "rollback;").unwrap();
}

#[test]
fn auto_checkpoint_bounds_wal_and_respects_open_trxs() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("wal.bin");
    let db = Database::open(dir.path()).unwrap();
    // budget of one byte: every commit outgrows it immediately
    db.set_wal_checkpoint_threshold(1);
    db.execute_sql("create table t (id int);").unwrap();
    db.execute_sql("insert into t values (1);").unwrap();
    assert_eq!(
        std::fs::metadata(&wal_path).unwrap().len(),
        0,
        "auto checkpoint after commit"
    );

    // while another session's transaction is open, the log must NOT be cut:
    // a later COMMIT of that trx needs its records in the log
    let mut open_trx = chaoticdb::Session::new();
    db.execute_sql_with(&mut open_trx, "begin;").unwrap();
    db.execute_sql_with(&mut open_trx, "insert into t values (2);").unwrap();
    db.execute_sql_with(&mut chaoticdb::Session::new(), "insert into t values (3);").unwrap();
    assert!(
        std::fs::metadata(&wal_path).unwrap().len() > 0,
        "no auto checkpoint while a transaction is open"
    );

    db.execute_sql_with(&mut open_trx, "commit;").unwrap();
    db.execute_sql_with(&mut chaoticdb::Session::new(), "insert into t values (4);").unwrap();
    assert_eq!(std::fs::metadata(&wal_path).unwrap().len(), 0, "resumes once idle");
    assert_eq!(
        rows(&db, "select * from t order by id;"),
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)],
            vec![Value::Int(4)],
        ]
    );
}

#[test]
fn dml_commits_do_not_rewrite_the_catalog() {
    // The catalog is saved by DDL and by checkpoints, not by every commit; a
    // long run of DML must not grow it (the old code rewrote the whole
    // committed-id list into it on each commit).
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    db.execute_sql("create table t (id int);").unwrap();
    let catalog = dir.path().join("catalog.bin");
    let after_ddl = std::fs::metadata(&catalog).unwrap().len();

    for i in 0..500 {
        db.execute_sql(&format!("insert into t values ({i});")).unwrap();
    }
    assert_eq!(
        std::fs::metadata(&catalog).unwrap().len(),
        after_ddl,
        "DML must not rewrite catalog.bin"
    );

    // the rows are still durable through the WAL
    drop(db);
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&db, "select id from t;").len(), 500);
}

#[test]
fn flush_during_open_transaction_is_rejected() {
    // flushing (and thus truncating the log) while a transaction is open
    // would drop its redo records, losing the COMMIT on recovery
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    db.execute_sql("create table t (id int);").unwrap();
    let mut session = chaoticdb::Session::new();
    db.execute_sql_with(&mut session, "begin;").unwrap();
    db.execute_sql_with(&mut session, "insert into t values (1);").unwrap();
    let err = db.flush().unwrap_err();
    assert!(err.to_string().contains("transaction"), "{err}");

    // the transaction still commits durably afterwards
    db.execute_sql_with(&mut session, "commit;").unwrap();
    drop(db);
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&db, "select * from t;"), vec![vec![Value::Int(1)]]);
}

#[test]
fn rollback_leaves_no_redo() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int);").unwrap();
        db.execute_sql("begin; insert into t values (1); rollback;").unwrap();
        db.flush().unwrap();
    }
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&db, "select * from t;"), Vec::<Vec<Value>>::new());
}

#[test]
fn concurrent_checkpoints_keep_every_commit_durable() {
    // Auto-checkpoint fires after every commit, so concurrent writers make
    // `flush_inner` run concurrently. The whole checkpoint must be atomic, or
    // one thread can truncate the log between another's engine flush and its
    // own write, losing redo that a later COMMIT depends on.
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(Database::open(dir.path()).unwrap());
    db.set_wal_checkpoint_threshold(1);
    db.execute_sql("create table t (id int);").unwrap();

    const THREADS: usize = 4;
    const PER_THREAD: usize = 25;
    let threads: Vec<_> = (0..THREADS)
        .map(|t| {
            let db = std::sync::Arc::clone(&db);
            std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    let id = t * PER_THREAD + i;
                    db.execute_sql(&format!("insert into t values ({id});")).unwrap();
                }
            })
        })
        .collect();
    for handle in threads {
        handle.join().unwrap();
    }

    let expected = THREADS * PER_THREAD;
    assert_eq!(rows(&db, "select id from t;").len(), expected);

    // every committed row is durable: reopen replays whatever the checkpoints
    // left in the log
    drop(db);
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&db, "select id from t;").len(), expected);
}

#[test]
fn explicit_flush_does_not_drop_a_concurrent_commits_redo() {
    // A direct `Database::flush` takes no database lock, so it can run while a
    // writer commits through the same `&Database`. If the flush truncates the
    // log after the commit but before the commit's pages are written back, a
    // crash loses a committed row. The flush must notice the commit and keep
    // the log.
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(dir.path()).unwrap());
    db.execute_sql("create table t (id int);").unwrap();

    const ROWS: usize = 200;
    let stop = Arc::new(AtomicBool::new(false));
    let flusher = {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match db.flush() {
                    Ok(()) => {}
                    // the writer keeps transactions in flight; that is expected
                    Err(e) if e.to_string().contains("transaction") => {}
                    Err(e) => panic!("unexpected flush error: {e}"),
                }
                std::thread::yield_now();
            }
        })
    };

    {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            for i in 0..ROWS {
                db.execute_sql(&format!("insert into t values ({i});")).unwrap();
            }
        })
    }
    .join()
    .unwrap();

    stop.store(true, Ordering::Relaxed);
    flusher.join().unwrap();

    // crash on purpose: only the WAL plus whatever the flushes persisted remain
    let db = Arc::try_unwrap(db).ok().expect("no other Arc holders remain");
    db.simulate_crash();

    let db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&db, "select id from t;").len(), ROWS);
}

#[test]
fn aborted_trx_id_is_not_reused_after_dirty_pages_were_evicted() {
    // An aborted transaction that never reached the log used to lose its id:
    // `next_trx_id` is only persisted by checkpoints, and a transaction that
    // never commits writes no WAL frame, so recovery's `max_trx_id` could not
    // see it. Its orphan pages may have been evicted to disk, so after a crash
    // a later transaction could be handed the same id, commit, and make those
    // never-committed rows visible. The `Begin` frame closes that hole.
    let mut cfg = Config::default();
    // A tiny pool so the aborted transaction's dirty pages are evicted while
    // its redo is still buffered in memory (and lost with the crash).
    cfg.storage.buffer_pool_frames = 4;
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open_with_config(dir.path(), &cfg).unwrap();
        db.execute_sql("create table t (id int);").unwrap();
        db.execute_sql("insert into t values (1);").unwrap();

        let mut s = Session::new();
        db.execute_sql_with(&mut s, "begin;").unwrap();
        for i in 0..2000i64 {
            db.execute_sql_with(&mut s, &format!("insert into t values ({});", 1000 + i))
                .unwrap();
        }
        // never commit: crash with the transaction open
        db.simulate_crash();
    }

    let db = Database::open_with_config(dir.path(), &cfg).unwrap();
    let count = |sql: &str| match db.execute_sql(sql).unwrap().remove(0) {
        ResultSet::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            ref other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    assert_eq!(count("select count(*) from t where id >= 1000 and id < 2000;"), 0);

    // Enough new transactions that the aborted id would be reached again.
    for i in 0..50i64 {
        db.execute_sql(&format!("insert into t values ({});", 9000 + i)).unwrap();
    }
    assert_eq!(
        count("select count(*) from t where id >= 1000 and id < 2000;"),
        0,
        "orphan rows became visible through xid reuse"
    );
}

#[test]
fn index_lookup_works_after_crash_recovery() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int primary key, name char(10));").unwrap();
        db.execute_sql("insert into t values (1, 'a'), (2, 'b'), (3, 'c');").unwrap();
        db.execute_sql("create index idx_name on t (name);").unwrap();
        // only the WAL survives; the derived index pages are rebuilt on open
        db.simulate_crash();
    }
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(
        rows(&db, "select name from t where id = 2;"),
        vec![vec![Value::Str("b".into())]]
    );
    assert_eq!(
        rows(&db, "select id from t where name = 'c';"),
        vec![vec![Value::Int(3)]]
    );
}
