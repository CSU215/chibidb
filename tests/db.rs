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

fn setup(db: &mut Database, ddl: &str) {
    db.execute_sql(ddl).unwrap();
}

#[test]
fn inserts_rows() {
    let mut db = Database::open_in_memory();
    setup(&mut db, "create table t (id int, name char(10), score float);");
    let rs = db
        .execute_sql("insert into t values (1, 'alice', 95.5), (2, 'bob', 80);")
        .unwrap();
    assert_eq!(rs, [ResultSet::Message("SUCCESS".into())]);
}

#[test]
fn insert_unknown_table_errors() {
    let mut db = Database::open_in_memory();
    let err = db.execute_sql("insert into t values (1);").unwrap_err();
    assert!(err.to_string().contains("no such table"), "{err}");
}

#[test]
fn insert_column_count_mismatch_errors() {
    let mut db = Database::open_in_memory();
    setup(&mut db, "create table t (id int, name char(10));");
    let err = db.execute_sql("insert into t values (1);").unwrap_err();
    assert!(err.to_string().contains("expected 2 values, got 1"), "{err}");
}

#[test]
fn insert_type_rules() {
    let mut db = Database::open_in_memory();
    setup(&mut db, "create table t (i int, f float, s char(4));");
    db.execute_sql("insert into t values (1, 2, 'ab');").unwrap();
    db.execute_sql("insert into t values (-1, 2.5, 'abcd');").unwrap();
    // int promotes into float column
    db.execute_sql("insert into t values (1, 3, 'x');").unwrap();
    // float does not fit into int column
    let err = db.execute_sql("insert into t values (1.5, 1, 'x');").unwrap_err();
    assert!(err.to_string().contains("cannot insert"), "{err}");
    // string does not fit into int column
    let err = db.execute_sql("insert into t values ('a', 1, 'x');").unwrap_err();
    assert!(err.to_string().contains("cannot insert"), "{err}");
    // number does not fit into char column
    let err = db.execute_sql("insert into t values (1, 1, 2);").unwrap_err();
    assert!(err.to_string().contains("cannot insert"), "{err}");
    // string longer than char(n)
    let err = db.execute_sql("insert into t values (1, 1, 'abcde');").unwrap_err();
    assert!(err.to_string().contains("cannot insert"), "{err}");
}

fn seeded() -> Database {
    let mut db = Database::open_in_memory();
    setup(&mut db, "create table student (id int, name char(10), score float);");
    db.execute_sql(
        "insert into student values (1, 'alice', 95.5), (2, 'bob', 80), (3, 'carol', 90);",
    )
    .unwrap();
    db
}

#[test]
fn selects_all_columns() {
    let mut db = seeded();
    let rs = db.execute_sql("select * from student;").unwrap();
    let (columns, rows) = rows(&rs);
    assert_eq!(columns, ["id", "name", "score"]);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], [Value::Int(1), Value::Str("alice".into()), Value::Float(95.5)]);
}

#[test]
fn selects_projected_columns_with_where() {
    let mut db = seeded();
    let rs = db
        .execute_sql("select name, score from student where score >= 90;")
        .unwrap();
    let (columns, rows) = rows(&rs);
    assert_eq!(columns, ["name", "score"]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], [Value::Str("alice".into()), Value::Float(95.5)]);
    assert_eq!(rows[1], [Value::Str("carol".into()), Value::Float(90.0)]);
}

#[test]
fn selects_expressions_over_columns() {
    let mut db = seeded();
    let rs = db.execute_sql("select id * 2 from student where id = 2;").unwrap();
    let (columns, rows) = rows(&rs);
    assert_eq!(columns, ["(* id 2)"]);
    assert_eq!(rows, [[Value::Int(4)]]);
}

#[test]
fn selects_empty_table_yields_header_only() {
    let mut db = Database::open_in_memory();
    setup(&mut db, "create table t (id int);");
    let rs = db.execute_sql("select * from t;").unwrap();
    let (columns, rows) = rows(&rs);
    assert_eq!(columns, ["id"]);
    assert_eq!(rows.len(), 0);
}

#[test]
fn select_from_errors() {
    let mut db = seeded();
    let err = db.execute_sql("select * from missing;").unwrap_err();
    assert!(err.to_string().contains("no such table"), "{err}");
    let err = db.execute_sql("select nope from student;").unwrap_err();
    assert!(err.to_string().contains("no such column"), "{err}");
    let err = db.execute_sql("select * from student where 1;").unwrap_err();
    assert!(err.to_string().contains("boolean"), "{err}");
}

#[test]
fn deletes_matching_rows() {
    let mut db = seeded();
    db.execute_sql("delete from student where id = 2;").unwrap();
    let rs = db.execute_sql("select id from student;").unwrap();
    let (_, rows) = rows(&rs);
    assert_eq!(rows, [[Value::Int(1)], [Value::Int(3)]]);
}

#[test]
fn deletes_all_rows_without_where() {
    let mut db = seeded();
    db.execute_sql("delete from student;").unwrap();
    let rs = db.execute_sql("select id from student;").unwrap();
    let (_, rows) = rows(&rs);
    assert_eq!(rows.len(), 0);
}

#[test]
fn delete_errors() {
    let mut db = seeded();
    let err = db.execute_sql("delete from missing;").unwrap_err();
    assert!(err.to_string().contains("no such table"), "{err}");
    let err = db.execute_sql("delete from student where name;").unwrap_err();
    assert!(err.to_string().contains("boolean"), "{err}");
}

#[test]
fn updates_matching_rows() {
    let mut db = seeded();
    db.execute_sql("update student set score = 100 where id = 2;").unwrap();
    let rs = db.execute_sql("select score from student where id = 2;").unwrap();
    let (_, rows) = rows(&rs);
    assert_eq!(rows, [[Value::Float(100.0)]]);
}

#[test]
fn updates_with_row_expressions() {
    let mut db = seeded();
    db.execute_sql("update student set score = score + 1 where id <= 2;").unwrap();
    let rs = db.execute_sql("select score from student;").unwrap();
    let (_, rows) = rows(&rs);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Value::Float(96.5));
    assert_eq!(rows[1][0], Value::Float(81.0));
    assert_eq!(rows[2][0], Value::Float(90.0), "unmatched row untouched");
}

#[test]
fn updates_without_where_touches_all_rows() {
    let mut db = seeded();
    db.execute_sql("update student set id = id * 10;").unwrap();
    let rs = db.execute_sql("select id from student;").unwrap();
    let (_, rows) = rows(&rs);
    assert_eq!(rows, [[Value::Int(10)], [Value::Int(20)], [Value::Int(30)]]);
}

#[test]
fn update_errors() {
    let mut db = seeded();
    let err = db.execute_sql("update missing set id = 1;").unwrap_err();
    assert!(err.to_string().contains("no such table"), "{err}");
    let err = db.execute_sql("update student set nope = 1;").unwrap_err();
    assert!(err.to_string().contains("no such column"), "{err}");
    let err = db.execute_sql("update student set name = 1;").unwrap_err();
    assert!(err.to_string().contains("cannot insert"), "{err}");
    let err = db.execute_sql("update student set name = 'waytoolongname';").unwrap_err();
    assert!(err.to_string().contains("cannot insert"), "{err}");
    let err = db.execute_sql("update student set id = 1 where name;").unwrap_err();
    assert!(err.to_string().contains("boolean"), "{err}");
}
