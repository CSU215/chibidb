use chibidb::ast::{CreateDatabaseStmt, DropDatabaseStmt, Stmt, UseStmt};
use chibidb::parser::parse;

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
