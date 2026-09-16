use chaoticdb::value::Value;
use chaoticdb::{Database, ResultSet};

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute_sql(sql).unwrap().remove(0) {
        ResultSet::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn message(db: &Database, sql: &str) -> String {
    match db.execute_sql(sql).unwrap().remove(0) {
        ResultSet::Message(m) => m,
        other => panic!("expected message, got {other:?}"),
    }
}

fn seed(db: &Database) {
    db.execute_sql("create table t (id int, name char(8));").unwrap();
    db.execute_sql("insert into t values (1, 'a'), (2, 'b'), (3, 'c');").unwrap();
}

#[test]
fn vacuum_purges_committed_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    seed(&db);
    db.execute_sql("delete from t where id = 2;").unwrap();

    let m = message(&db, "vacuum;");
    assert!(m.contains("1 rows purged"), "{m}");

    // visible state unchanged by the purge
    assert_eq!(
        rows(&db, "select * from t order by id;"),
        vec![
            vec![Value::Int(1), Value::Str("a".into())],
            vec![Value::Int(3), Value::Str("c".into())],
        ]
    );

    // vacuuming again has nothing to do
    let m = message(&db, "vacuum;");
    assert!(m.contains("0 rows purged"), "{m}");
}

#[test]
fn vacuum_cleans_stale_index_entries() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    seed(&db);
    db.execute_sql("create index idx_id on t (id);").unwrap();
    db.execute_sql("delete from t where id = 2;").unwrap();
    db.execute_sql("vacuum;").unwrap();

    // the stale index entry must not survive: an index scan over the purged
    // rid has to return empty, not fail with "no record at rid"
    let plan = message(&db, "explain select * from t where id = 2;");
    assert!(plan.contains("IndexScan"), "plan: {plan}");
    assert_eq!(rows(&db, "select * from t where id = 2;"), Vec::<Vec<Value>>::new());

    // surviving rows remain reachable through the index
    assert_eq!(
        rows(&db, "select * from t where id = 1;"),
        vec![vec![Value::Int(1), Value::Str("a".into())]]
    );
    assert_eq!(
        rows(&db, "select * from t where id = 3;"),
        vec![vec![Value::Int(3), Value::Str("c".into())]]
    );
}

#[test]
fn vacuum_is_crash_safe() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        seed(&db);
        db.execute_sql("delete from t where id = 2;").unwrap();
        db.execute_sql("vacuum;").unwrap();
        // dirty pages are lost, only the WAL survives; recovery replays the
        // original insert + delete-mark, yielding the same visible state
        db.simulate_crash();
    }
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(
        rows(&db, "select * from t order by id;"),
        vec![
            vec![Value::Int(1), Value::Str("a".into())],
            vec![Value::Int(3), Value::Str("c".into())],
        ]
    );
    // and the recovered state is stable across another reopen
    drop(db);
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(
        rows(&db, "select * from t order by id;"),
        vec![
            vec![Value::Int(1), Value::Str("a".into())],
            vec![Value::Int(3), Value::Str("c".into())],
        ]
    );
}

#[test]
fn vacuum_then_slot_reuse_survives_a_crash() {
    // VACUUM physically frees a rid without logging it; a later insert can
    // reuse that rid immediately. Before VACUUM checkpointed, a crash after
    // the reuse would replay the new row into a slot that the un-flushed purge
    // still showed as occupied, silently dropping the new row.
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        seed(&db);
        db.execute_sql("delete from t where id = 2;").unwrap();
        db.execute_sql("vacuum;").unwrap();
        // reuse the freed slot on the same page, then crash with the new row
        // still only in the (replayed) log
        db.execute_sql("insert into t values (4, 'd');").unwrap();
        db.simulate_crash();
    }
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(
        rows(&db, "select * from t order by id;"),
        vec![
            vec![Value::Int(1), Value::Str("a".into())],
            vec![Value::Int(3), Value::Str("c".into())],
            vec![Value::Int(4), Value::Str("d".into())],
        ]
    );
}

#[test]
fn vacuum_rejects_open_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    seed(&db);
    let mut session = chaoticdb::Session::new();
    db.execute_sql_with(&mut session, "begin;").unwrap();
    let err = db.execute_sql_with(&mut session, "vacuum;").unwrap_err();
    assert!(err.to_string().contains("transaction"), "{err}");
    db.execute_sql_with(&mut session, "rollback;").unwrap();
    // after the rollback vacuum works again
    let m = message(&db, "vacuum;");
    assert!(m.contains("0 rows purged"), "{m}");
}

#[test]
fn vacuum_horizon_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        seed(&db);
        // accumulate history, then compact it into the horizon
        for _ in 0..20 {
            db.execute_sql("update t set name = 'x' where id = 1;").unwrap();
        }
        db.execute_sql("delete from t where id = 2;").unwrap();
        let m = message(&db, "vacuum;");
        assert!(m.contains("1 rows purged"), "{m}");
    }
    // the frozen horizon is persisted, so the compacted clog reloads correctly
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(
        rows(&db, "select * from t order by id;"),
        vec![
            vec![Value::Int(1), Value::Str("x".into())],
            vec![Value::Int(3), Value::Str("c".into())],
        ]
    );
    // new writes land above the horizon and stay visible
    db.execute_sql("insert into t values (4, 'd');").unwrap();
    assert_eq!(
        rows(&db, "select * from t order by id;"),
        vec![
            vec![Value::Int(1), Value::Str("x".into())],
            vec![Value::Int(3), Value::Str("c".into())],
            vec![Value::Int(4), Value::Str("d".into())],
        ]
    );
}

#[test]
fn vacuum_purges_rows_marked_by_other_committed_trxs() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    seed(&db);
    // delete two rows in one explicit transaction, then commit
    let mut session = chaoticdb::Session::new();
    db.execute_sql_with(&mut session, "begin;").unwrap();
    db.execute_sql_with(&mut session, "delete from t where id in (1, 3);").unwrap();
    db.execute_sql_with(&mut session, "commit;").unwrap();

    let m = message(&db, "vacuum;");
    assert!(m.contains("2 rows purged"), "{m}");
    assert_eq!(rows(&db, "select * from t;"), vec![vec![Value::Int(2), Value::Str("b".into())]]);
}
