use chibidb::value::Value;
use chibidb::{Database, ResultSet};

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute_sql(sql).unwrap().remove(0) {
        ResultSet::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn messages(db: &mut Database, sql: &str) -> String {
    match db.execute_sql(sql).unwrap().remove(0) {
        ResultSet::Message(m) => m,
        other => panic!("expected message, got {other:?}"),
    }
}

#[test]
fn committed_insert_survives_crash() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int, name char(10));").unwrap();
        db.execute_sql("insert into t values (1, 'alice'), (2, 'bob');").unwrap();
        // no flush: dirty pages stay in the buffer pool, only the WAL is durable
        db.simulate_crash();
    }
    let mut db = Database::open(dir.path()).unwrap();
    assert_eq!(
        rows(&mut db, "select * from t order by id;"),
        vec![
            vec![Value::Int(1), Value::Str("alice".into())],
            vec![Value::Int(2), Value::Str("bob".into())],
        ]
    );
}

#[test]
fn committed_delete_survives_crash() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int);").unwrap();
        db.execute_sql("insert into t values (1), (2);").unwrap();
        db.flush().unwrap();
        db.execute_sql("delete from t where id = 1;").unwrap();
        db.simulate_crash();
    }
    let mut db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&mut db, "select * from t order by id;"), vec![vec![Value::Int(2)]]);
}

#[test]
fn committed_update_survives_crash() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int, score float);").unwrap();
        db.execute_sql("insert into t values (1, 10.0), (2, 20.0);").unwrap();
        db.flush().unwrap();
        db.execute_sql("update t set score = score + 1 where id = 1;").unwrap();
        db.simulate_crash();
    }
    let mut db = Database::open(dir.path()).unwrap();
    assert_eq!(
        rows(&mut db, "select * from t order by id;"),
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
        let mut db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int, name char(10));").unwrap();
        db.execute_sql("begin; insert into t values (1, 'ghost');").unwrap();
        // crash with the transaction still open
        db.simulate_crash();
    }
    let mut db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&mut db, "select * from t;"), Vec::<Vec<Value>>::new());

    // the aborted trx id must not be reused, otherwise the next transaction
    // would adopt the ghost row as its own
    db.execute_sql("insert into t values (2, 'real');").unwrap();
    assert_eq!(
        rows(&mut db, "select * from t order by id;"),
        vec![vec![Value::Int(2), Value::Str("real".into())]]
    );

    // and the recovered state must be stable across another crash/reopen
    drop(db);
    let mut db = Database::open(dir.path()).unwrap();
    assert_eq!(
        rows(&mut db, "select * from t order by id;"),
        vec![vec![Value::Int(2), Value::Str("real".into())]]
    );
}

#[test]
fn committed_indexed_insert_survives_crash() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int, name char(10));").unwrap();
        db.execute_sql("create index idx_id on t (id);").unwrap();
        db.execute_sql("insert into t values (1, 'alice'), (2, 'bob');").unwrap();
        db.simulate_crash();
    }
    let mut db = Database::open(dir.path()).unwrap();
    let plan = messages(&mut db, "explain select * from t where id = 1;");
    assert!(plan.contains("IndexScan"), "plan: {plan}");
    assert_eq!(
        rows(&mut db, "select * from t where id = 2;"),
        vec![vec![Value::Int(2), Value::Str("bob".into())]]
    );
}

#[test]
fn truncated_tail_frame_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = Database::open(dir.path()).unwrap();
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

    let mut db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&mut db, "select * from t;"), vec![vec![Value::Int(7)]]);
}

#[test]
fn clean_flush_truncates_wal() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("wal.bin");
    {
        let mut db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int);").unwrap();
        db.execute_sql("insert into t values (1);").unwrap();
        assert!(std::fs::metadata(&wal_path).unwrap().len() > 0, "wal must hold redo records");
        db.flush().unwrap();
        assert_eq!(std::fs::metadata(&wal_path).unwrap().len(), 0, "clean flush is a checkpoint");
    }
    let mut db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&mut db, "select * from t;"), vec![vec![Value::Int(1)]]);
}

#[test]
fn drop_table_survives_crash_with_stale_wal() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int);").unwrap();
        db.execute_sql("insert into t values (1);").unwrap();
        db.execute_sql("drop table t;").unwrap();
        // the log still holds records for the dropped table's file
        db.simulate_crash();
    }
    // recovery must skip stale records of the dropped table, not fail
    let mut db = Database::open(dir.path()).unwrap();
    let err = db.execute_sql("select * from t;").unwrap_err();
    assert!(err.to_string().contains("no such table"), "{err}");
    db.execute_sql("create table t (id int);").unwrap();
    db.execute_sql("insert into t values (5);").unwrap();
    assert_eq!(rows(&mut db, "select * from t;"), vec![vec![Value::Int(5)]]);
}

#[test]
fn checkpoint_statement_flushes_and_truncates_wal() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("wal.bin");
    let mut db = Database::open(dir.path()).unwrap();
    db.execute_sql("create table t (id int);").unwrap();
    db.execute_sql("insert into t values (1);").unwrap();
    assert!(std::fs::metadata(&wal_path).unwrap().len() > 0, "precondition: wal has records");

    db.execute_sql("checkpoint;").unwrap();

    assert_eq!(std::fs::metadata(&wal_path).unwrap().len(), 0, "checkpoint clears the log");
    assert_eq!(rows(&mut db, "select * from t;"), vec![vec![Value::Int(1)]]);
    drop(db);
    let mut db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&mut db, "select * from t;"), vec![vec![Value::Int(1)]]);
}

#[test]
fn checkpoint_rejects_open_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path()).unwrap();
    db.execute_sql("create table t (id int);").unwrap();
    let mut session = chibidb::Session::new();
    db.execute_sql_with(&mut session, "begin;").unwrap();
    let err = db.execute_sql_with(&mut session, "checkpoint;").unwrap_err();
    assert!(err.to_string().contains("transaction"), "{err}");
    db.execute_sql_with(&mut session, "rollback;").unwrap();
}

#[test]
fn auto_checkpoint_bounds_wal_and_respects_open_trxs() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("wal.bin");
    let mut db = Database::open(dir.path()).unwrap();
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
    let mut open_trx = chibidb::Session::new();
    db.execute_sql_with(&mut open_trx, "begin;").unwrap();
    db.execute_sql_with(&mut open_trx, "insert into t values (2);").unwrap();
    db.execute_sql_with(&mut chibidb::Session::new(), "insert into t values (3);").unwrap();
    assert!(
        std::fs::metadata(&wal_path).unwrap().len() > 0,
        "no auto checkpoint while a transaction is open"
    );

    db.execute_sql_with(&mut open_trx, "commit;").unwrap();
    db.execute_sql_with(&mut chibidb::Session::new(), "insert into t values (4);").unwrap();
    assert_eq!(std::fs::metadata(&wal_path).unwrap().len(), 0, "resumes once idle");
    assert_eq!(
        rows(&mut db, "select * from t order by id;"),
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)],
            vec![Value::Int(4)],
        ]
    );
}

#[test]
fn flush_during_open_transaction_is_rejected() {
    // flushing (and thus truncating the log) while a transaction is open
    // would drop its redo records, losing the COMMIT on recovery
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path()).unwrap();
    db.execute_sql("create table t (id int);").unwrap();
    let mut session = chibidb::Session::new();
    db.execute_sql_with(&mut session, "begin;").unwrap();
    db.execute_sql_with(&mut session, "insert into t values (1);").unwrap();
    let err = db.flush().unwrap_err();
    assert!(err.to_string().contains("transaction"), "{err}");

    // the transaction still commits durably afterwards
    db.execute_sql_with(&mut session, "commit;").unwrap();
    drop(db);
    let mut db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&mut db, "select * from t;"), vec![vec![Value::Int(1)]]);
}

#[test]
fn rollback_leaves_no_redo() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int);").unwrap();
        db.execute_sql("begin; insert into t values (1); rollback;").unwrap();
        db.flush().unwrap();
    }
    let mut db = Database::open(dir.path()).unwrap();
    assert_eq!(rows(&mut db, "select * from t;"), Vec::<Vec<Value>>::new());
}
