use chibidb::value::Value;
use chibidb::{Database, ResultSet};

fn rows(rs: &[ResultSet]) -> (&[String], &[Vec<Value>]) {
    match &rs[0] {
        ResultSet::Rows { columns, rows } => (columns, rows),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn executes_constant_select() {
    let mut db = Database::open_in_memory();
    let rs = db.execute_sql("select 1+2, 'ab';").unwrap();
    assert_eq!(rs.len(), 1);
    let (columns, rows) = rows(&rs);
    assert_eq!(columns, ["(+ 1 2)", "'ab'"]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], [Value::Int(3), Value::Str("ab".into())]);
}

#[test]
fn executes_each_statement() {
    let mut db = Database::open_in_memory();
    let rs = db.execute_sql("select 1; select 2;").unwrap();
    assert_eq!(rs.len(), 2);
    let (_, r1) = rows(&rs[0..1]);
    assert_eq!(r1[0][0], Value::Int(1));
    let (_, r2) = rows(&rs[1..2]);
    assert_eq!(r2[0][0], Value::Int(2));
}

#[test]
fn empty_sql_yields_no_results() {
    let mut db = Database::open_in_memory();
    assert_eq!(db.execute_sql("").unwrap().len(), 0);
    assert_eq!(db.execute_sql(";;").unwrap().len(), 0);
}

#[test]
fn star_requires_from() {
    let mut db = Database::open_in_memory();
    assert!(db.execute_sql("select *;").is_err());
}

#[test]
fn runtime_errors_propagate() {
    let mut db = Database::open_in_memory();
    let err = db.execute_sql("select 1/0;").unwrap_err();
    assert!(err.to_string().contains("division by zero"), "{err}");
}

#[test]
fn unimplemented_statements_error() {
    let mut db = Database::open_in_memory();
    assert!(db.execute_sql("insert into t values (1);").is_err());
    assert!(db.execute_sql("select 1 from t;").is_err());
}

#[test]
fn creates_table() {
    let mut db = Database::open_in_memory();
    let rs = db
        .execute_sql("create table t (id int, name char(10), score float);")
        .unwrap();
    assert_eq!(rs, [ResultSet::Message("SUCCESS".into())]);
}

#[test]
fn duplicate_table_errors() {
    let mut db = Database::open_in_memory();
    db.execute_sql("create table t (id int);").unwrap();
    let err = db.execute_sql("create table t (id int);").unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
}

#[test]
fn table_names_are_case_sensitive() {
    let mut db = Database::open_in_memory();
    db.execute_sql("create table t (id int);").unwrap();
    let err = db.execute_sql("create table T (id int);");
    assert!(err.is_ok(), "distinct names should both work");
    let err = db.execute_sql("create table t (x int);").unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
}
