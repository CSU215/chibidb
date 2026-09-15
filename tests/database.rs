use chaoticdb::sql::ast::{CreateDatabaseStmt, DropDatabaseStmt, ShowColumnsStmt, Stmt, UseStmt};
use chaoticdb::config::Config;
use chaoticdb::instance::Instance;
use chaoticdb::sql::parser::parse;
use chaoticdb::value::Value;
use chaoticdb::{ResultSet, Session};

fn instance(dir: &tempfile::TempDir) -> Instance {
    Instance::open(dir.path(), &Config::default()).unwrap()
}

fn first_int(rs: &[ResultSet]) -> i64 {
    match &rs[0] {
        ResultSet::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            ref v => panic!("expected int, got {v:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

fn one(sql: &str) -> Stmt {
    parse(sql).unwrap().remove(0)
}

#[test]
fn parses_database_statements() {
    assert_eq!(
        one("create database shop;"),
        Stmt::CreateDatabase(CreateDatabaseStmt { name: "shop".into() })
    );
    assert_eq!(
        one("drop database shop;"),
        Stmt::DropDatabase(DropDatabaseStmt { name: "shop".into() })
    );
    assert_eq!(one("use shop;"), Stmt::Use(UseStmt { name: "shop".into() }));
}

#[test]
fn database_keywords_are_case_insensitive() {
    assert_eq!(
        one("CREATE DATABASE Shop;"),
        Stmt::CreateDatabase(CreateDatabaseStmt { name: "Shop".into() })
    );
    assert_eq!(one("USE Shop;"), Stmt::Use(UseStmt { name: "Shop".into() }));
}

#[test]
fn rejects_malformed_database_statements() {
    assert!(parse("create database;").is_err());
    assert!(parse("use;").is_err());
    assert!(parse("drop;").is_err());
}

#[test]
fn create_use_and_query_across_databases() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();

    inst.execute_with(&mut s, "create database shop;").unwrap();
    inst.execute_with(&mut s, "use shop;").unwrap();
    assert_eq!(s.current_db(), Some("shop"));
    inst.execute_with(&mut s, "create table t (id int);").unwrap();
    inst.execute_with(&mut s, "insert into t values (5);").unwrap();
    assert_eq!(first_int(&inst.execute_with(&mut s, "select id from t;").unwrap()), 5);

    inst.execute_with(&mut s, "create database blog;").unwrap();
    inst.execute_with(&mut s, "use blog;").unwrap();
    assert!(inst.execute_with(&mut s, "select id from t;").is_err());
}

#[test]
fn table_statements_auto_select_a_default_database() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();

    // no USE needed: the first table statement lands in `main`
    inst.execute_with(&mut s, "create table t (id int);").unwrap();
    assert_eq!(s.current_db(), Some("main"));
    inst.execute_with(&mut s, "insert into t values (1);").unwrap();
    assert_eq!(
        first_int(&inst.execute_with(&mut s, "select count(*) from t;").unwrap()),
        1
    );

    // an explicit database can still be selected and isolated
    inst.execute_with(&mut s, "create database other;").unwrap();
    inst.execute_with(&mut s, "use other;").unwrap();
    assert_eq!(s.current_db(), Some("other"));
    assert!(inst.execute_with(&mut s, "select * from t;").is_err());
}

#[test]
fn use_unknown_database_errors() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();
    assert!(inst.execute_with(&mut s, "use nope;").is_err());
    assert_eq!(s.current_db(), None);
}

#[test]
fn dropped_database_cannot_be_queried() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();
    inst.execute_with(&mut s, "create database shop;").unwrap();
    inst.execute_with(&mut s, "use shop;").unwrap();
    inst.execute_with(&mut s, "drop database shop;").unwrap();
    assert!(inst.execute_with(&mut s, "select 1;").is_err());
}

#[test]
fn database_statements_are_rejected_inside_a_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();
    inst.execute_with(&mut s, "create database shop;").unwrap();
    inst.execute_with(&mut s, "use shop;").unwrap();
    inst.execute_with(&mut s, "begin;").unwrap();
    assert!(inst.execute_with(&mut s, "create database other;").is_err());
}

fn column_strings(rs: &[ResultSet]) -> Vec<String> {
    match &rs[0] {
        ResultSet::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r[0] {
                Value::Str(s) => s.clone(),
                v => panic!("expected string, got {v:?}"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn parses_show_statements() {
    assert_eq!(one("show tables;"), Stmt::ShowTables);
    assert_eq!(one("show databases;"), Stmt::ShowDatabases);
    assert_eq!(one("SHOW DATABASES;"), Stmt::ShowDatabases);
    assert!(parse("show;").is_err());
    assert!(parse("show columns;").is_err());
}

#[test]
fn show_databases_lists_registered_databases() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();
    inst.execute_with(&mut s, "create database shop;").unwrap();
    inst.execute_with(&mut s, "create database blog;").unwrap();
    let rs = inst.execute_with(&mut s, "show databases;").unwrap();
    assert_eq!(columns_and_rows(&rs).0, ["database"].map(String::from).as_slice());
    assert_eq!(column_strings(&rs), ["blog", "information_schema", "shop"]);
}

#[test]
fn show_tables_lists_tables_and_views() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();
    inst.execute_with(&mut s, "create table t (id int);").unwrap();
    inst.execute_with(&mut s, "create table u (id int);").unwrap();
    inst.execute_with(&mut s, "create view v as select id from t;").unwrap();
    let rs = inst.execute_with(&mut s, "show tables;").unwrap();
    assert_eq!(columns_and_rows(&rs).0, ["table"].map(String::from).as_slice());
    assert_eq!(column_strings(&rs), ["t", "u", "v"]);
}

fn columns_and_rows(rs: &[ResultSet]) -> (&[String], &[Vec<Value>]) {
    match &rs[0] {
        ResultSet::Rows { columns, rows } => (columns, rows),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn empty() -> Value {
    Value::Str(String::new())
}

#[test]
fn parses_show_columns_and_describe() {
    for sql in ["show columns from t;", "show columns in t;", "describe t;", "desc t;"] {
        assert_eq!(
            one(sql),
            Stmt::ShowColumns(ShowColumnsStmt { table: "t".into() }),
            "{sql}"
        );
    }
    assert!(parse("show columns;").is_err());
    assert!(parse("show columns t;").is_err());
    assert!(parse("describe;").is_err());
}

#[test]
fn show_columns_describes_a_table() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();
    inst.execute_with(
        &mut s,
        "create table t (id int primary key, name char(5) not null unique default 'x', score float);",
    )
    .unwrap();

    let rs = inst.execute_with(&mut s, "show columns from t;").unwrap();
    let (cols, rows) = columns_and_rows(&rs);
    assert_eq!(
        cols,
        ["field", "type", "null", "key", "default", "extra"]
            .map(String::from)
            .as_slice()
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0],
        vec![
            Value::Str("id".into()),
            Value::Str("int".into()),
            Value::Str("NO".into()),
            Value::Str("PRI".into()),
            Value::Null,
            empty(),
        ]
    );
    assert_eq!(
        rows[1],
        vec![
            Value::Str("name".into()),
            Value::Str("char(5)".into()),
            Value::Str("NO".into()),
            Value::Str("UNI".into()),
            Value::Str("x".into()),
            empty(),
        ]
    );
    assert_eq!(
        rows[2],
        vec![
            Value::Str("score".into()),
            Value::Str("float".into()),
            Value::Str("YES".into()),
            Value::Str("".into()),
            Value::Null,
            empty(),
        ]
    );

    // DESCRIBE is the same as SHOW COLUMNS
    let rs = inst.execute_with(&mut s, "describe t;").unwrap();
    assert_eq!(columns_and_rows(&rs).1, rows);
}

#[test]
fn show_columns_rejects_views_and_unknown_tables() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();
    inst.execute_with(&mut s, "create table t (id int);").unwrap();
    inst.execute_with(&mut s, "create view v as select id from t;").unwrap();
    let err = inst.execute_with(&mut s, "describe v;").unwrap_err().to_string();
    assert!(err.contains("view"), "{err}");
    let err = inst.execute_with(&mut s, "describe nope;").unwrap_err().to_string();
    assert!(err.contains("no such table"), "{err}");
}
