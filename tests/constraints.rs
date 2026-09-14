//! M20 column constraints: NOT NULL, DEFAULT, and INSERT column lists.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chibidb::config::{Config, Isolation};
use chibidb::value::Value;
use chibidb::{Database, ResultSet, Session};

fn rows(rs: &[ResultSet]) -> Vec<Vec<Value>> {
    match &rs[0] {
        ResultSet::Rows { rows, .. } => rows.clone(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn q(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    let rs = db.execute_sql(sql).unwrap();
    rows(&rs)
}

fn err(db: &Database, sql: &str) -> String {
    db.execute_sql(sql).unwrap_err().to_string()
}

#[test]
fn not_null_is_enforced_on_insert_and_update() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int not null, name char(10));").unwrap();

    let e = err(&db, "insert into t values (null, 'a');");
    assert!(e.contains("cannot be null"), "{e}");

    db.execute_sql("insert into t values (1, 'a');").unwrap();
    let e = err(&db, "update t set id = null where name = 'a';");
    assert!(e.contains("cannot be null"), "{e}");
}

#[test]
fn default_fills_omitted_columns() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql(
        "create table t (id int, age int default 7, city char(8) default 'unk');",
    )
    .unwrap();

    db.execute_sql("insert into t (id) values (1);").unwrap();
    assert_eq!(
        q(&db, "select id, age, city from t;"),
        [[Value::Int(1), Value::Int(7), Value::Str("unk".into())]]
    );

    // an explicit NULL overrides the default for a nullable column
    db.execute_sql("insert into t (id, age) values (2, null);").unwrap();
    assert_eq!(
        q(&db, "select id, age from t where id = 2;"),
        [[Value::Int(2), Value::Null]]
    );
}

#[test]
fn insert_column_list_validates_and_reorders() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (a int, b int);").unwrap();

    let e = err(&db, "insert into t (c) values (1);");
    assert!(e.contains("no such column"), "{e}");
    let e = err(&db, "insert into t (a, a) values (1, 2);");
    assert!(e.contains("twice"), "{e}");
    let e = err(&db, "insert into t (a) values (1, 2);");
    assert!(e.contains("expected 1 values"), "{e}");

    // values map to the named columns, not positional order
    db.execute_sql("insert into t (b, a) values (10, 20);").unwrap();
    assert_eq!(q(&db, "select a, b from t;"), [[Value::Int(20), Value::Int(10)]]);
}

#[test]
fn primary_key_rejects_null_and_duplicates() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int primary key, name char(10));").unwrap();

    let e = err(&db, "insert into t values (null, 'a');");
    assert!(e.contains("cannot be null"), "{e}");

    db.execute_sql("insert into t values (1, 'a');").unwrap();
    let e = err(&db, "insert into t values (1, 'b');");
    assert!(e.contains("duplicate key"), "{e}");

    // duplicate within a single multi-row statement
    let e = err(&db, "insert into t values (2, 'b'), (2, 'c');");
    assert!(e.contains("duplicate key"), "{e}");
}

#[test]
fn unique_allows_multiple_nulls_but_not_duplicates() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int, email char(20) unique);").unwrap();

    db.execute_sql("insert into t values (1, null), (2, null);").unwrap();
    db.execute_sql("insert into t values (3, 'x');").unwrap();
    let e = err(&db, "insert into t values (4, 'x');");
    assert!(e.contains("duplicate key"), "{e}");
}

#[test]
fn unique_is_enforced_on_update() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int, email char(20) unique);").unwrap();
    db.execute_sql("insert into t values (1, 'a'), (2, 'b');").unwrap();

    let e = err(&db, "update t set email = 'b' where id = 1;");
    assert!(e.contains("duplicate key"), "{e}");

    // same value and NULL remain allowed
    db.execute_sql("update t set email = 'a' where id = 1;").unwrap();
    db.execute_sql("update t set email = null where id = 1;").unwrap();
}

#[test]
fn uniqueness_is_checked_per_column_not_across_constraints() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int primary key, ref int unique);").unwrap();
    // the same encoded value in two DIFFERENT unique columns of one row is legal
    db.execute_sql("insert into t values (1, 1);").unwrap();

    // multi-row: values may repeat across distinct constraint columns
    db.execute_sql("create table u (a int unique, b int unique);").unwrap();
    db.execute_sql("insert into u values (1, 2), (2, 1);").unwrap();

    // real duplicates are still rejected
    let e = err(&db, "insert into t values (1, 3);");
    assert!(e.contains("duplicate key"), "{e}");
    let e = err(&db, "insert into u values (1, 9);");
    assert!(e.contains("duplicate key: u(a)"), "{e}");
}

#[test]
fn constraint_index_cannot_be_dropped() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int primary key);").unwrap();
    let e = err(&db, "drop index __unique_t_id;");
    assert!(e.contains("cannot drop"), "{e}");
    // a plain user index is still droppable
    db.execute_sql("create index idx on t (id);").unwrap();
    db.execute_sql("drop index idx;").unwrap();
}

#[test]
fn constraints_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int not null, age int default 7);").unwrap();
        db.execute_sql("insert into t (id) values (1);").unwrap();
    }
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(q(&db, "select id, age from t;"), [[Value::Int(1), Value::Int(7)]]);
    let e = err(&db, "insert into t (age) values (3);");
    assert!(e.contains("cannot be null"), "{e}");
}

#[test]
fn unique_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int primary key);").unwrap();
        db.execute_sql("insert into t values (1);").unwrap();
    }
    let db = Database::open(dir.path()).unwrap();
    let e = err(&db, "insert into t values (1);");
    assert!(e.contains("duplicate key"), "{e}");
    db.execute_sql("insert into t values (2);").unwrap();
}

/// One winner, seven duplicate-key errors: the per-key lock serializes the
/// uniqueness check, so the losers see the winner's committed row.
fn concurrent_duplicate_key_race(isolation: Isolation) {
    let mut cfg = Config::default();
    cfg.transaction.isolation = isolation;
    let db = Arc::new(Database::open_in_memory_with_config(&cfg).unwrap());
    db.execute_sql("create table t (id int primary key, n int);").unwrap();

    let rejected = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let db = Arc::clone(&db);
            let rejected = Arc::clone(&rejected);
            scope.spawn(move || {
                let mut session = Session::new();
                if db.execute_sql_with(&mut session, "insert into t values (1, 0);").is_err() {
                    rejected.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });

    assert_eq!(q(&db, "select count(*) from t;"), [[Value::Int(1)]], "{isolation:?} duplicated");
    assert_eq!(rejected.load(Ordering::SeqCst), 7, "{isolation:?}: losing inserts must be rejected");
}

#[test]
fn concurrent_duplicate_keys_are_rejected_read_committed() {
    concurrent_duplicate_key_race(Isolation::ReadCommitted);
}

#[test]
fn concurrent_duplicate_keys_are_rejected_repeatable_read() {
    concurrent_duplicate_key_race(Isolation::RepeatableRead);
}

#[test]
fn concurrent_duplicate_keys_are_rejected_serializable() {
    concurrent_duplicate_key_race(Isolation::Serializable);
}

#[test]
fn concurrent_distinct_keys_all_succeed() {
    let db = Arc::new(Database::open_in_memory().unwrap());
    db.execute_sql("create table t (id int primary key);").unwrap();

    std::thread::scope(|scope| {
        for i in 0..8u64 {
            let db = Arc::clone(&db);
            scope.spawn(move || {
                let mut session = Session::new();
                db.execute_sql_with(&mut session, &format!("insert into t values ({i});")).unwrap();
            });
        }
    });
    assert_eq!(q(&db, "select count(*) from t;"), [[Value::Int(8)]]);
}

#[test]
fn concurrent_unique_column_update_is_rejected() {
    // two sessions race to move different rows onto the same unique value
    let db = Arc::new(Database::open_in_memory().unwrap());
    db.execute_sql("create table t (id int primary key, v int unique);").unwrap();
    db.execute_sql("insert into t values (1, 10), (2, 20);").unwrap();

    let rejected = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for id in [1, 2] {
            let db = Arc::clone(&db);
            let rejected = Arc::clone(&rejected);
            scope.spawn(move || {
                let mut session = Session::new();
                if db
                    .execute_sql_with(&mut session, &format!("update t set v = 99 where id = {id};"))
                    .is_err()
                {
                    rejected.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });

    assert_eq!(
        q(&db, "select count(*) from t where v = 99;"),
        [[Value::Int(1)]],
        "only one update may take the unique value"
    );
    assert_eq!(rejected.load(Ordering::SeqCst), 1);
}
