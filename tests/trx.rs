use chaoticdb::config::{Config, Isolation};
use chaoticdb::value::Value;
use chaoticdb::{Database, ResultSet, Session};

fn short_lock_config() -> Config {
    let mut cfg = Config::default();
    cfg.transaction.lock_timeout_ms = 100;
    cfg
}

fn repeatable_read_config() -> Config {
    let mut cfg = Config::default();
    cfg.transaction.isolation = Isolation::RepeatableRead;
    cfg
}

fn serializable_config() -> Config {
    let mut cfg = Config::default();
    cfg.transaction.isolation = Isolation::Serializable;
    cfg
}

fn setup(db: &Database, session: &mut Session) {
    db.execute_sql_with(session, "create table t (id int, name char(8));")
        .unwrap();
}

fn rows(db: &Database, session: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match &db.execute_sql_with(session, sql).unwrap()[0] {
        ResultSet::Rows { rows, .. } => rows.clone(),
        other => panic!("expected rows for {sql}, got {other:?}"),
    }
}

fn err(db: &Database, session: &mut Session, sql: &str) {
    assert!(db.execute_sql_with(session, sql).is_err(), "expected error: {sql}");
}

#[test]
fn uncommitted_inserts_are_invisible_to_others() {
    let db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&db, &mut a);

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "insert into t values (1, 'x');").unwrap();

    // writer sees its own writes
    assert_eq!(rows(&db, &mut a, "select count(*) from t;"), [[Value::Int(1)]]);
    // other session does not
    assert_eq!(rows(&db, &mut b, "select count(*) from t;"), [[Value::Int(0)]]);
    // autocommit session does not either
    assert_eq!(rows(&db, &mut b, "select count(*) from t;"), [[Value::Int(0)]]);

    db.execute_sql_with(&mut a, "commit;").unwrap();
    assert_eq!(rows(&db, &mut b, "select count(*) from t;"), [[Value::Int(1)]]);
}

#[test]
fn ddl_is_blocked_while_another_transaction_is_open() {
    let db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&db, &mut a);

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
    let db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&db, &mut a);

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "insert into t values (1, 'x');").unwrap();
    db.execute_sql_with(&mut a, "rollback;").unwrap();

    assert_eq!(rows(&db, &mut a, "select count(*) from t;"), [[Value::Int(0)]]);
    assert_eq!(rows(&db, &mut b, "select count(*) from t;"), [[Value::Int(0)]]);
}

#[test]
fn repeatable_read_snapshots_are_stable_within_a_transaction() {
    let db = Database::open_in_memory_with_config(&repeatable_read_config()).unwrap();
    let mut reader = Session::new();
    let mut writer = Session::new();
    setup(&db, &mut reader);
    db.execute_sql_with(&mut reader, "insert into t values (1, 'a');").unwrap();

    // reader opens a snapshot transaction: sees 1 row
    db.execute_sql_with(&mut reader, "begin;").unwrap();
    assert_eq!(rows(&db, &mut reader, "select count(*) from t;"), [[Value::Int(1)]]);

    // writer commits a second row after the snapshot
    db.execute_sql_with(&mut writer, "insert into t values (2, 'b');").unwrap();

    // reader still sees exactly its snapshot
    assert_eq!(rows(&db, &mut reader, "select count(*) from t;"), [[Value::Int(1)]]);
    db.execute_sql_with(&mut reader, "commit;").unwrap();

    // fresh reads see both rows
    assert_eq!(rows(&db, &mut reader, "select count(*) from t;"), [[Value::Int(2)]]);
}

#[test]
fn read_committed_sees_a_concurrent_commit_per_statement() {
    // the default isolation: a statement sees the latest committed data, so a
    // transaction does not repeat its read.
    let db = Database::open_in_memory().unwrap();
    let mut reader = Session::new();
    let mut writer = Session::new();
    setup(&db, &mut reader);
    db.execute_sql_with(&mut reader, "insert into t values (1, 'a');").unwrap();

    db.execute_sql_with(&mut reader, "begin;").unwrap();
    assert_eq!(rows(&db, &mut reader, "select count(*) from t;"), [[Value::Int(1)]]);

    db.execute_sql_with(&mut writer, "insert into t values (2, 'b');").unwrap();

    assert_eq!(rows(&db, &mut reader, "select count(*) from t;"), [[Value::Int(2)]]);
    db.execute_sql_with(&mut reader, "commit;").unwrap();
}

#[test]
fn uncommitted_deletes_are_invisible_to_others() {
    let db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&db, &mut a);
    db.execute_sql_with(&mut a, "insert into t values (1, 'x');").unwrap();

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "delete from t;").unwrap();
    // deleter sees it gone
    assert_eq!(rows(&db, &mut a, "select count(*) from t;"), [[Value::Int(0)]]);
    // others still see it
    assert_eq!(rows(&db, &mut b, "select count(*) from t;"), [[Value::Int(1)]]);
    db.execute_sql_with(&mut a, "commit;").unwrap();
    assert_eq!(rows(&db, &mut b, "select count(*) from t;"), [[Value::Int(0)]]);
}

#[test]
fn rollback_restores_delete_marks() {
    let db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&db, &mut a);
    db.execute_sql_with(&mut a, "insert into t values (1, 'x');").unwrap();

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "delete from t;").unwrap();
    db.execute_sql_with(&mut a, "rollback;").unwrap();
    assert_eq!(rows(&db, &mut b, "select count(*) from t;"), [[Value::Int(1)]]);
    assert_eq!(rows(&db, &mut a, "select count(*) from t;"), [[Value::Int(1)]]);
}

#[test]
fn uncommitted_updates_are_invisible_to_others() {
    let db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&db, &mut a);
    db.execute_sql_with(&mut a, "insert into t values (1, 'old');").unwrap();

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "update t set name = 'new' where id = 1;").unwrap();
    assert_eq!(rows(&db, &mut a, "select name from t;"), [[Value::Str("new".into())]]);
    assert_eq!(rows(&db, &mut b, "select name from t;"), [[Value::Str("old".into())]]);
    db.execute_sql_with(&mut a, "commit;").unwrap();
    assert_eq!(rows(&db, &mut b, "select name from t;"), [[Value::Str("new".into())]]);
}

#[test]
fn rollback_restores_updates() {
    let db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let _b = Session::new();
    setup(&db, &mut a);
    db.execute_sql_with(&mut a, "insert into t values (1, 'old');").unwrap();

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "update t set name = 'new' where id = 1;").unwrap();
    db.execute_sql_with(&mut a, "rollback;").unwrap();
    assert_eq!(rows(&db, &mut a, "select name from t;"), [[Value::Str("old".into())]]);
}

#[test]
fn autocommit_statements_are_their_own_transactions() {
    let db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&db, &mut a);
    // statement errors roll back their own partial work
    err(&db, &mut a, "insert into t values (1, 'ok'), (2, 'waytoolong');");

    db.execute_sql_with(&mut a, "begin;").unwrap();
    err(&db, &mut a, "insert into t values (3, 'ok'), (4, 'waytoolong');");
    db.execute_sql_with(&mut a, "commit;").unwrap();

    assert_eq!(rows(&db, &mut b, "select count(*) from t;"), [[Value::Int(0)]],
        "failed statements are rolled back completely");
}

#[test]
fn transaction_control_errors() {
    let db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&db, &mut a);

    err(&db, &mut a, "commit;");
    err(&db, &mut a, "rollback;");

    db.execute_sql_with(&mut a, "begin;").unwrap();
    err(&db, &mut a, "begin;");
    db.execute_sql_with(&mut a, "commit;").unwrap();

    // independent transactions don't block each other
    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut b, "begin;").unwrap();
    db.execute_sql_with(&mut a, "commit;").unwrap();
    db.execute_sql_with(&mut b, "commit;").unwrap();
}

#[test]
fn repeatable_read_aborts_the_lost_update() {
    use std::sync::Arc;
    use std::sync::mpsc;

    let db = Arc::new(Database::open_in_memory_with_config(&repeatable_read_config()).unwrap());
    let mut a = Session::new();
    setup(&db, &mut a);
    db.execute_sql_with(&mut a, "insert into t values (1, 'base');").unwrap();

    // A updates the row and holds its row lock until COMMIT.
    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "update t set name = 'a' where id = 1;").unwrap();

    // B begins (snapshot before A commits) and blocks updating the same row.
    let (begun_tx, begun_rx) = mpsc::channel();
    let b = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            let mut b = Session::new();
            db.execute_sql_with(&mut b, "begin;").unwrap();
            begun_tx.send(()).unwrap();
            let update = db.execute_sql_with(&mut b, "update t set name = 'b' where id = 1;");
            let commit = db.execute_sql_with(&mut b, "commit;");
            (update.err().map(|e| e.to_string()), commit.err().map(|e| e.to_string()))
        })
    };
    begun_rx.recv().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    // A commits, releasing the row lock; B's update then runs on its stale
    // snapshot and its commit is rejected.
    db.execute_sql_with(&mut a, "commit;").unwrap();
    let (update_err, commit_err) = b.join().unwrap();
    assert!(update_err.is_none(), "update should not fail: {update_err:?}");
    let commit_err = commit_err.expect("B's commit must fail");
    assert!(commit_err.contains("serialize"), "{commit_err}");

    // the loser's value never lands
    assert_eq!(rows(&db, &mut a, "select name from t;"), [[Value::Str("a".into())]]);
    assert_eq!(rows(&db, &mut a, "select count(*) from t;"), [[Value::Int(1)]]);
}

#[test]
fn read_committed_reapplies_after_a_concurrent_update() {
    use std::sync::Arc;
    use std::sync::mpsc;

    let db = Arc::new(Database::open_in_memory().unwrap());
    let mut a = Session::new();
    db.execute_sql_with(&mut a, "create table c (id int, n int);").unwrap();
    db.execute_sql_with(&mut a, "insert into c values (1, 100);").unwrap();

    // A increments and holds its row lock until COMMIT.
    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "update c set n = n + 1 where id = 1;").unwrap();

    // B begins (snapshot before A commits) and blocks on the same row.
    let (begun_tx, begun_rx) = mpsc::channel();
    let b = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            let mut b = Session::new();
            db.execute_sql_with(&mut b, "begin;").unwrap();
            begun_tx.send(()).unwrap();
            let update = db.execute_sql_with(&mut b, "update c set n = n + 1 where id = 1;");
            let commit = db.execute_sql_with(&mut b, "commit;");
            (update.err().map(|e| e.to_string()), commit.err().map(|e| e.to_string()))
        })
    };
    begun_rx.recv().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    db.execute_sql_with(&mut a, "commit;").unwrap();
    let (update_err, commit_err) = b.join().unwrap();

    // read committed restarts B's update on the committed value instead of
    // aborting, so neither increment is lost.
    assert!(update_err.is_none(), "update should retry, not fail: {update_err:?}");
    assert!(commit_err.is_none(), "commit should succeed: {commit_err:?}");
    assert_eq!(rows(&db, &mut a, "select n from c;"), [[Value::Int(102)]]);
}

#[test]
fn read_committed_skips_a_row_the_concurrent_commit_moved_out_of_the_predicate() {
    use std::sync::Arc;
    use std::sync::mpsc;

    let db = Arc::new(Database::open_in_memory().unwrap());
    let mut a = Session::new();
    db.execute_sql_with(&mut a, "create table c (id int, n int);").unwrap();
    db.execute_sql_with(&mut a, "insert into c values (1, 100);").unwrap();

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "update c set n = 0 where id = 1;").unwrap();

    let (begun_tx, begun_rx) = mpsc::channel();
    let b = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            let mut b = Session::new();
            db.execute_sql_with(&mut b, "begin;").unwrap();
            begun_tx.send(()).unwrap();
            // matches at B's snapshot (n = 100), but not after A commits (n = 0)
            let update = db.execute_sql_with(&mut b, "update c set n = n + 1 where id = 1 and n > 50;");
            let commit = db.execute_sql_with(&mut b, "commit;");
            (update.err().map(|e| e.to_string()), commit.err().map(|e| e.to_string()))
        })
    };
    begun_rx.recv().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    db.execute_sql_with(&mut a, "commit;").unwrap();
    let (update_err, commit_err) = b.join().unwrap();

    assert!(update_err.is_none(), "update should retry, not fail: {update_err:?}");
    assert!(commit_err.is_none(), "commit should succeed: {commit_err:?}");
    // the restarted statement re-evaluated the predicate and skipped the row
    assert_eq!(rows(&db, &mut a, "select n from c;"), [[Value::Int(0)]]);
}

#[test]
fn read_committed_keeps_independent_updates() {
    let db = Database::open_in_memory().unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&db, &mut a);
    db.execute_sql_with(&mut a, "insert into t values (1, 'x'), (2, 'y');").unwrap();

    // transactions touch different rows: no conflict
    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "update t set name = 'a' where id = 1;").unwrap();
    db.execute_sql_with(&mut b, "begin;").unwrap();
    db.execute_sql_with(&mut b, "update t set name = 'b' where id = 2;").unwrap();
    db.execute_sql_with(&mut a, "commit;").unwrap();
    db.execute_sql_with(&mut b, "commit;").unwrap();

    assert_eq!(
        rows(&db, &mut a, "select name from t order by id;"),
        [[Value::Str("a".into())], [Value::Str("b".into())]]
    );
}

#[test]
fn row_locks_serialize_writers_and_time_out() {
    let db = Database::open_in_memory_with_config(&short_lock_config()).unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&db, &mut a);
    db.execute_sql_with(&mut a, "insert into t values (1, 'base');").unwrap();

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "update t set name = 'a' where id = 1;").unwrap();

    // BEGIN is not blocked (there is no whole-database lock), but b's write to
    // the same row waits on a's row lock and times out.
    db.execute_sql_with(&mut b, "begin;").unwrap();
    let err = db.execute_sql_with(&mut b, "update t set name = 'b' where id = 1;").unwrap_err();
    assert!(err.to_string().contains("lock wait timeout"), "{err}");

    // once a commits, b's retry runs against the latest row and succeeds
    db.execute_sql_with(&mut a, "commit;").unwrap();
    db.execute_sql_with(&mut b, "update t set name = 'b' where id = 1;").unwrap();
    db.execute_sql_with(&mut b, "commit;").unwrap();
    assert_eq!(rows(&db, &mut b, "select name from t;"), [[Value::Str("b".into())]]);
    assert_eq!(rows(&db, &mut b, "select count(*) from t;"), [[Value::Int(1)]]);
}

#[test]
fn row_locks_cover_autocommit_writers() {
    let db = Database::open_in_memory_with_config(&short_lock_config()).unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&db, &mut a);
    db.execute_sql_with(&mut a, "insert into t values (1, 'base');").unwrap();

    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "update t set name = 'a' where id = 1;").unwrap();

    // an autocommit write to the *same* row waits on the row lock and times out
    let err = db.execute_sql_with(&mut b, "update t set name = 'x' where id = 1;").unwrap_err();
    assert!(err.to_string().contains("lock wait timeout"), "{err}");
    // a different row is unaffected: locks are per row, not per database
    db.execute_sql_with(&mut b, "insert into t values (2, 'x');").unwrap();

    db.execute_sql_with(&mut a, "commit;").unwrap();
    assert_eq!(rows(&db, &mut b, "select count(*) from t;"), [[Value::Int(2)]]);
}

#[test]
fn readonly_error_in_explicit_transaction_keeps_the_transaction() {
    let db = Database::open_in_memory().unwrap();
    let mut s = Session::new();
    setup(&db, &mut s);

    db.execute_sql_with(&mut s, "begin;").unwrap();
    db.execute_sql_with(&mut s, "insert into t values (1, 'x');").unwrap();

    // a failing read-only statement must not drop the transaction handle
    err(&db, &mut s, "select * from no_such_table;");

    // the transaction is still open and its earlier work commits
    db.execute_sql_with(&mut s, "commit;").unwrap();
    assert_eq!(rows(&db, &mut s, "select count(*) from t;"), [[Value::Int(1)]]);

    // bookkeeping is clean: an open-trx guard would block this DDL
    db.execute_sql_with(&mut s, "create table u (id int);").unwrap();
}

#[test]
fn failed_statement_preserves_earlier_transaction_work() {
    let db = Database::open_in_memory().unwrap();
    let mut s = Session::new();
    setup(&db, &mut s);

    db.execute_sql_with(&mut s, "begin;").unwrap();
    db.execute_sql_with(&mut s, "insert into t values (1, 'ok');").unwrap();

    // the second insert fails: 'way-too-long' exceeds char(8)
    err(&db, &mut s, "insert into t values (2, 'way-too-long');");

    // a statement-level failure must not roll back the earlier insert
    db.execute_sql_with(&mut s, "commit;").unwrap();
    assert_eq!(rows(&db, &mut s, "select id from t;"), [[Value::Int(1)]]);
}

#[test]
fn serializable_rejects_write_skew() {
    let db = Database::open_in_memory_with_config(&serializable_config()).unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    db.execute_sql_with(&mut a, "create table t (id int, on_call int);").unwrap();
    db.execute_sql_with(&mut a, "insert into t values (1, 1), (2, 1);").unwrap();

    // each transaction reads the whole table (both doctors on call), then takes
    // a different doctor off call: a classic write skew.
    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "select count(*) from t where on_call = 1;").unwrap();
    db.execute_sql_with(&mut a, "update t set on_call = 0 where id = 1;").unwrap();

    db.execute_sql_with(&mut b, "begin;").unwrap();
    db.execute_sql_with(&mut b, "select count(*) from t where on_call = 1;").unwrap();
    db.execute_sql_with(&mut b, "update t set on_call = 0 where id = 2;").unwrap();

    // the first committer closes the rw cycle and is rejected
    let a_err = db.execute_sql_with(&mut a, "commit;").unwrap_err();
    assert!(a_err.to_string().contains("serialize"), "{a_err}");
    // the other transaction survives, so one doctor stays on call
    db.execute_sql_with(&mut b, "commit;").unwrap();
    assert_eq!(
        rows(&db, &mut b, "select count(*) from t where on_call = 1;"),
        [[Value::Int(1)]]
    );
}

#[test]
fn serializable_allows_a_read_only_and_a_writer() {
    let db = Database::open_in_memory_with_config(&serializable_config()).unwrap();
    let mut reader = Session::new();
    let mut writer = Session::new();
    db.execute_sql_with(&mut reader, "create table t (id int);").unwrap();
    db.execute_sql_with(&mut reader, "insert into t values (1);").unwrap();

    // reader holds a snapshot and has read t, but writes nothing
    db.execute_sql_with(&mut reader, "begin;").unwrap();
    assert_eq!(rows(&db, &mut reader, "select count(*) from t;"), [[Value::Int(1)]]);

    db.execute_sql_with(&mut writer, "insert into t values (2);").unwrap();

    // a read does not conflict with a concurrent writer on its own
    db.execute_sql_with(&mut reader, "commit;").unwrap();
}

#[test]
fn serializable_keeps_independent_transactions() {
    let db = Database::open_in_memory_with_config(&serializable_config()).unwrap();
    let mut a = Session::new();
    let mut b = Session::new();
    setup(&db, &mut a);
    db.execute_sql_with(&mut a, "insert into t values (1, 'x'), (2, 'y');").unwrap();

    // touching different tables leaves the dependency graph acyclic
    db.execute_sql_with(&mut a, "create table u (id int);").unwrap();
    db.execute_sql_with(&mut a, "begin;").unwrap();
    db.execute_sql_with(&mut a, "update t set name = 'a' where id = 1;").unwrap();
    db.execute_sql_with(&mut b, "begin;").unwrap();
    db.execute_sql_with(&mut b, "insert into u values (1);").unwrap();
    db.execute_sql_with(&mut a, "commit;").unwrap();
    db.execute_sql_with(&mut b, "commit;").unwrap();
}
