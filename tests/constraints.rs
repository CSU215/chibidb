//! M20 column constraints: NOT NULL, DEFAULT, and INSERT column lists.

use chibidb::value::Value;
use chibidb::{Database, ResultSet};

fn rows(rs: &[ResultSet]) -> Vec<Vec<Value>> {
    match &rs[0] {
        ResultSet::Rows { rows, .. } => rows.clone(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn q(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    let rs = db.execute_sql(sql).unwrap();
    rows(&rs)
}

fn err(db: &mut Database, sql: &str) -> String {
    db.execute_sql(sql).unwrap_err().to_string()
}

#[test]
fn not_null_is_enforced_on_insert_and_update() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int not null, name char(10));").unwrap();

    let e = err(&mut db, "insert into t values (null, 'a');");
    assert!(e.contains("cannot be null"), "{e}");

    db.execute_sql("insert into t values (1, 'a');").unwrap();
    let e = err(&mut db, "update t set id = null where name = 'a';");
    assert!(e.contains("cannot be null"), "{e}");
}

#[test]
fn default_fills_omitted_columns() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute_sql(
        "create table t (id int, age int default 7, city char(8) default 'unk');",
    )
    .unwrap();

    db.execute_sql("insert into t (id) values (1);").unwrap();
    assert_eq!(
        q(&mut db, "select id, age, city from t;"),
        [[Value::Int(1), Value::Int(7), Value::Str("unk".into())]]
    );

    // an explicit NULL overrides the default for a nullable column
    db.execute_sql("insert into t (id, age) values (2, null);").unwrap();
    assert_eq!(
        q(&mut db, "select id, age from t where id = 2;"),
        [[Value::Int(2), Value::Null]]
    );
}

#[test]
fn insert_column_list_validates_and_reorders() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (a int, b int);").unwrap();

    let e = err(&mut db, "insert into t (c) values (1);");
    assert!(e.contains("no such column"), "{e}");
    let e = err(&mut db, "insert into t (a, a) values (1, 2);");
    assert!(e.contains("twice"), "{e}");
    let e = err(&mut db, "insert into t (a) values (1, 2);");
    assert!(e.contains("expected 1 values"), "{e}");

    // values map to the named columns, not positional order
    db.execute_sql("insert into t (b, a) values (10, 20);").unwrap();
    assert_eq!(q(&mut db, "select a, b from t;"), [[Value::Int(20), Value::Int(10)]]);
}

#[test]
fn constraints_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int not null, age int default 7);").unwrap();
        db.execute_sql("insert into t (id) values (1);").unwrap();
    }
    let mut db = Database::open(dir.path()).unwrap();
    assert_eq!(q(&mut db, "select id, age from t;"), [[Value::Int(1), Value::Int(7)]]);
    let e = err(&mut db, "insert into t (age) values (3);");
    assert!(e.contains("cannot be null"), "{e}");
}
