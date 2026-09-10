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
