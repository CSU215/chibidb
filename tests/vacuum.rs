use chibidb::value::Value;
use chibidb::{Database, ResultSet};

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute_sql(sql).unwrap().remove(0) {
        ResultSet::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn message(db: &mut Database, sql: &str) -> String {
    match db.execute_sql(sql).unwrap().remove(0) {
        ResultSet::Message(m) => m,
        other => panic!("expected message, got {other:?}"),
    }
}

fn seed(db: &mut Database) {
    db.execute_sql("create table t (id int, name char(8));").unwrap();
    db.execute_sql("insert into t values (1, 'a'), (2, 'b'), (3, 'c');").unwrap();
}

#[test]
fn vacuum_purges_committed_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path()).unwrap();
    seed(&mut db);
    db.execute_sql("delete from t where id = 2;").unwrap();

    let m = message(&mut db, "vacuum;");
    assert!(m.contains("1 rows purged"), "{m}");

    // visible state unchanged by the purge
    assert_eq!(
        rows(&mut db, "select * from t order by id;"),
        vec![
            vec![Value::Int(1), Value::Str("a".into())],
            vec![Value::Int(3), Value::Str("c".into())],
        ]
    );

    // vacuuming again has nothing to do
    let m = message(&mut db, "vacuum;");
    assert!(m.contains("0 rows purged"), "{m}");
}

#[test]
fn vacuum_cleans_stale_index_entries() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path()).unwrap();
    seed(&mut db);
    db.execute_sql("create index idx_id on t (id);").unwrap();
    db.execute_sql("delete from t where id = 2;").unwrap();
    db.execute_sql("vacuum;").unwrap();

    // the stale index entry must not survive: an index scan over the purged
    // rid has to return empty, not fail with "no record at rid"
    let plan = message(&mut db, "explain select * from t where id = 2;");
    assert!(plan.contains("IndexScan"), "plan: {plan}");
    assert_eq!(rows(&mut db, "select * from t where id = 2;"), Vec::<Vec<Value>>::new());

    // surviving rows remain reachable through the index
    assert_eq!(
        rows(&mut db, "select * from t where id = 1;"),
        vec![vec![Value::Int(1), Value::Str("a".into())]]
    );
    assert_eq!(
        rows(&mut db, "select * from t where id = 3;"),
        vec![vec![Value::Int(3), Value::Str("c".into())]]
    );
}

#[test]
fn vacuum_is_crash_safe() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = Database::open(dir.path()).unwrap();
        seed(&mut db);
        db.execute_sql("delete from t where id = 2;").unwrap();
        db.execute_sql("vacuum;").unwrap();
        // dirty pages are lost, only the WAL survives; recovery replays the
        // original insert + delete-mark, yielding the same visible state
        db.simulate_crash();
    }
    let mut db = Database::open(dir.path()).unwrap();
    assert_eq!(
        rows(&mut db, "select * from t order by id;"),
        vec![
            vec![Value::Int(1), Value::Str("a".into())],
            vec![Value::Int(3), Value::Str("c".into())],
        ]
    );
    // and the recovered state is stable across another reopen
    drop(db);
    let mut db = Database::open(dir.path()).unwrap();
    assert_eq!(
        rows(&mut db, "select * from t order by id;"),
        vec![
            vec![Value::Int(1), Value::Str("a".into())],
            vec![Value::Int(3), Value::Str("c".into())],
        ]
    );
}

#[test]
fn vacuum_rejects_open_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path()).unwrap();
    seed(&mut db);
    let mut session = chibidb::Session::new();
    db.execute_sql_with(&mut session, "begin;").unwrap();
    let err = db.execute_sql_with(&mut session, "vacuum;").unwrap_err();
    assert!(err.to_string().contains("transaction"), "{err}");
    db.execute_sql_with(&mut session, "rollback;").unwrap();
    // after the rollback vacuum works again
    let m = message(&mut db, "vacuum;");
    assert!(m.contains("0 rows purged"), "{m}");
}

#[test]
fn vacuum_purges_rows_marked_by_other_committed_trxs() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path()).unwrap();
    seed(&mut db);
    // delete two rows in one explicit transaction, then commit
    let mut session = chibidb::Session::new();
    db.execute_sql_with(&mut session, "begin;").unwrap();
    db.execute_sql_with(&mut session, "delete from t where id in (1, 3);").unwrap();
    db.execute_sql_with(&mut session, "commit;").unwrap();

    let m = message(&mut db, "vacuum;");
    assert!(m.contains("2 rows purged"), "{m}");
    assert_eq!(rows(&mut db, "select * from t;"), vec![vec![Value::Int(2), Value::Str("b".into())]]);
}
