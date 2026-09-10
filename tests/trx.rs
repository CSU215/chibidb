use chibidb::value::Value;
use chibidb::{Database, ResultSet, Session};

fn setup(db: &mut Database, session: &mut Session) {
    db.execute_sql_with(session, "create table t (id int, name char(8));")
        .unwrap();
}

fn rows(db: &mut Database, session: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match &db.execute_sql_with(session, sql).unwrap()[0] {
        ResultSet::Rows { rows, .. } => rows.clone(),
        other => panic!("expected rows for {sql}, got {other:?}"),
    }
}

fn err(db: &mut Database, session: &mut Session, sql: &str) {
    assert!(db.execute_sql_with(session, sql).is_err(), "expected error: {sql}");
}

#[test]
fn uncommitted_inserts_are_invisible_to_others() {
    let mut db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&mut db, &mut a);

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "insert into t values (1, 'x');").unwrap();

    // writer sees its own writes
    assert_eq!(rows(&mut db, &mut a, "select count(*) from t;"), [[Value::Int(1)]]);
    // other session does not
    assert_eq!(rows(&mut db, &mut b, "select count(*) from t;"), [[Value::Int(0)]]);
    // autocommit session does not either
    assert_eq!(rows(&mut db, &mut b, "select count(*) from t;"), [[Value::Int(0)]]);

    db.execute_sql_with(&mut a, "commit;").unwrap();
    assert_eq!(rows(&mut db, &mut b, "select count(*) from t;"), [[Value::Int(1)]]);
}

#[test]
fn ddl_is_blocked_while_another_transaction_is_open() {
    let mut db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&mut db, &mut a);

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "insert into t values (1, 'x');").unwrap();

    for sql in [
        "drop table t;",
        "create table u (id int);",
        "create index i on t (id);",
        "drop index i;",
        "create view v as select id from t;",
        "drop view v;",
    ] {
        let err = db.execute_sql_with(&mut b, sql).unwrap_err();
        assert!(err.to_string().contains("locked"), "{sql}: {err}");
    }

    db.execute_sql_with(&mut a, "rollback;").unwrap();
    db.execute_sql_with(&mut b, "create table u (id int);").unwrap();
}

#[test]
fn rollback_discards_inserts() {
    let mut db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&mut db, &mut a);

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "insert into t values (1, 'x');").unwrap();
    db.execute_sql_with(&mut a, "rollback;").unwrap();

    assert_eq!(rows(&mut db, &mut a, "select count(*) from t;"), [[Value::Int(0)]]);
    assert_eq!(rows(&mut db, &mut b, "select count(*) from t;"), [[Value::Int(0)]]);
}

#[test]
fn snapshots_are_stable_within_a_transaction() {
    let mut db = Database::open_in_memory().unwrap();
    let mut reader = Session::new();
    let mut writer = Session::new();
    setup(&mut db, &mut reader);
    db.execute_sql_with(&mut reader, "insert into t values (1, 'a');").unwrap();

    // reader opens a snapshot transaction: sees 1 row
    db.execute_sql_with(&mut reader, "begin;").unwrap();
    assert_eq!(rows(&mut db, &mut reader, "select count(*) from t;"), [[Value::Int(1)]]);

    // writer commits a second row after the snapshot
    db.execute_sql_with(&mut writer, "insert into t values (2, 'b');").unwrap();

    // reader still sees exactly its snapshot
    assert_eq!(rows(&mut db, &mut reader, "select count(*) from t;"), [[Value::Int(1)]]);
    db.execute_sql_with(&mut reader, "commit;").unwrap();

    // fresh reads see both rows
    assert_eq!(rows(&mut db, &mut reader, "select count(*) from t;"), [[Value::Int(2)]]);
}

#[test]
fn uncommitted_deletes_are_invisible_to_others() {
    let mut db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&mut db, &mut a);
    db.execute_sql_with(&mut a, "insert into t values (1, 'x');").unwrap();

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "delete from t;").unwrap();
    // deleter sees it gone
    assert_eq!(rows(&mut db, &mut a, "select count(*) from t;"), [[Value::Int(0)]]);
    // others still see it
    assert_eq!(rows(&mut db, &mut b, "select count(*) from t;"), [[Value::Int(1)]]);
    db.execute_sql_with(&mut a, "commit;").unwrap();
    assert_eq!(rows(&mut db, &mut b, "select count(*) from t;"), [[Value::Int(0)]]);
}

#[test]
fn rollback_restores_delete_marks() {
    let mut db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&mut db, &mut a);
    db.execute_sql_with(&mut a, "insert into t values (1, 'x');").unwrap();

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "delete from t;").unwrap();
    db.execute_sql_with(&mut a, "rollback;").unwrap();
    assert_eq!(rows(&mut db, &mut b, "select count(*) from t;"), [[Value::Int(1)]]);
    assert_eq!(rows(&mut db, &mut a, "select count(*) from t;"), [[Value::Int(1)]]);
}

#[test]
fn uncommitted_updates_are_invisible_to_others() {
    let mut db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&mut db, &mut a);
    db.execute_sql_with(&mut a, "insert into t values (1, 'old');").unwrap();

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "update t set name = 'new' where id = 1;").unwrap();
    assert_eq!(rows(&mut db, &mut a, "select name from t;"), [[Value::Str("new".into())]]);
    assert_eq!(rows(&mut db, &mut b, "select name from t;"), [[Value::Str("old".into())]]);
    db.execute_sql_with(&mut a, "commit;").unwrap();
    assert_eq!(rows(&mut db, &mut b, "select name from t;"), [[Value::Str("new".into())]]);
}

#[test]
fn rollback_restores_updates() {
    let mut db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let _b = Session::new();
    setup(&mut db, &mut a);
    db.execute_sql_with(&mut a, "insert into t values (1, 'old');").unwrap();

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "update t set name = 'new' where id = 1;").unwrap();
    db.execute_sql_with(&mut a, "rollback;").unwrap();
    assert_eq!(rows(&mut db, &mut a, "select name from t;"), [[Value::Str("old".into())]]);
}

#[test]
fn autocommit_statements_are_their_own_transactions() {
    let mut db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&mut db, &mut a);
    // statement errors roll back their own partial work
    err(&mut db, &mut a, "insert into t values (1, 'ok'), (2, 'waytoolong');");

    db.execute_sql_with(&mut a, "begin;").unwrap();
    err(&mut db, &mut a, "insert into t values (3, 'ok'), (4, 'waytoolong');");
    db.execute_sql_with(&mut a, "commit;").unwrap();

    assert_eq!(rows(&mut db, &mut b, "select count(*) from t;"), [[Value::Int(0)]],
        "failed statements are rolled back completely");
}

#[test]
fn transaction_control_errors() {
    let mut db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&mut db, &mut a);

    err(&mut db, &mut a, "commit;");
    err(&mut db, &mut a, "rollback;");

    db.execute_sql_with(&mut a, "begin;").unwrap();
    err(&mut db, &mut a, "begin;");
    db.execute_sql_with(&mut a, "commit;").unwrap();

    // independent transactions don't block each other
    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut b, "begin;").unwrap();
    db.execute_sql_with(&mut a, "commit;").unwrap();
    db.execute_sql_with(&mut b, "commit;").unwrap();
}

