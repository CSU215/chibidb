use chibidb::ast::{CreateDatabaseStmt, DropDatabaseStmt, Stmt, UseStmt};
use chibidb::config::Config;
use chibidb::instance::Instance;
use chibidb::parser::parse;
use chibidb::value::Value;
use chibidb::{ResultSet, Session};

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
