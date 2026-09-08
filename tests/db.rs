use chibidb::value::Value;
use chibidb::{Database, ResultSet};

fn rows(rs: &[ResultSet]) -> (&[String], &[Vec<Value>]) {
    match &rs[0] {
        ResultSet::Rows { columns, rows } => (columns, rows),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Runs `f` against every backend: in-memory and file-backed.
fn with_dbs(f: impl Fn(&mut Database)) {
    let mut mem = Database::open_in_memory();
    f(&mut mem);

    let dir = tempfile::tempdir().unwrap();
    let mut file_db = Database::open(dir.path()).unwrap();
    f(&mut file_db);
}

fn seed(db: &mut Database) {
    db.execute_sql("create table student (id int, name char(10), score float);")
        .unwrap();
    db.execute_sql(
        "insert into student values (1, 'alice', 95.5), (2, 'bob', 80), (3, 'carol', 90);",
    )
    .unwrap();
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
fn creates_table() {
    with_dbs(|db| {
        let rs = db
            .execute_sql("create table t (id int, name char(10), score float);")
            .unwrap();
        assert_eq!(rs, [ResultSet::Message("SUCCESS".into())]);
    });
}

#[test]
fn duplicate_table_errors() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int);").unwrap();
        let err = db.execute_sql("create table t (id int);").unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    });
}

#[test]
fn table_names_are_case_sensitive() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int);").unwrap();
        let ok = db.execute_sql("create table T (id int);");
        assert!(ok.is_ok(), "distinct names should both work");
        let err = db.execute_sql("create table t (x int);").unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    });
}

#[test]
fn inserts_rows() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int, name char(10), score float);")
            .unwrap();
        let rs = db
            .execute_sql("insert into t values (1, 'alice', 95.5), (2, 'bob', 80);")
            .unwrap();
        assert_eq!(rs, [ResultSet::Message("SUCCESS".into())]);
    });
}

#[test]
fn insert_unknown_table_errors() {
    with_dbs(|db| {
        let err = db.execute_sql("insert into t values (1);").unwrap_err();
        assert!(err.to_string().contains("no such table"), "{err}");
    });
}

#[test]
fn insert_column_count_mismatch_errors() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int, name char(10));").unwrap();
        let err = db.execute_sql("insert into t values (1);").unwrap_err();
        assert!(err.to_string().contains("expected 2 values, got 1"), "{err}");
    });
}

#[test]
fn insert_type_rules() {
    with_dbs(|db| {
        db.execute_sql("create table t (i int, f float, s char(4));").unwrap();
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
    });
}

#[test]
fn selects_all_columns() {
    with_dbs(|db| {
        seed(db);
        let rs = db.execute_sql("select * from student;").unwrap();
        let (columns, rows) = rows(&rs);
        assert_eq!(columns, ["id", "name", "score"]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], [Value::Int(1), Value::Str("alice".into()), Value::Float(95.5)]);
    });
}

#[test]
fn selects_projected_columns_with_where() {
    with_dbs(|db| {
        seed(db);
        let rs = db
            .execute_sql("select name, score from student where score >= 90;")
            .unwrap();
        let (columns, rows) = rows(&rs);
        assert_eq!(columns, ["name", "score"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], [Value::Str("alice".into()), Value::Float(95.5)]);
        assert_eq!(rows[1], [Value::Str("carol".into()), Value::Float(90.0)]);
    });
}

#[test]
fn selects_expressions_over_columns() {
    with_dbs(|db| {
        seed(db);
        let rs = db.execute_sql("select id * 2 from student where id = 2;").unwrap();
        let (columns, rows) = rows(&rs);
        assert_eq!(columns, ["(* id 2)"]);
        assert_eq!(rows, [[Value::Int(4)]]);
    });
}

#[test]
fn selects_empty_table_yields_header_only() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int);").unwrap();
        let rs = db.execute_sql("select * from t;").unwrap();
        let (columns, rows) = rows(&rs);
        assert_eq!(columns, ["id"]);
        assert_eq!(rows.len(), 0);
    });
}

#[test]
fn select_from_errors() {
    with_dbs(|db| {
        seed(db);
        let err = db.execute_sql("select * from missing;").unwrap_err();
        assert!(err.to_string().contains("no such table"), "{err}");
        let err = db.execute_sql("select nope from student;").unwrap_err();
        assert!(err.to_string().contains("no such column"), "{err}");
        let err = db.execute_sql("select * from student where 1;").unwrap_err();
        assert!(err.to_string().contains("boolean"), "{err}");
    });
}

#[test]
fn deletes_matching_rows() {
    with_dbs(|db| {
        seed(db);
        db.execute_sql("delete from student where id = 2;").unwrap();
        let rs = db.execute_sql("select id from student;").unwrap();
        let (_, rows) = rows(&rs);
        assert_eq!(rows, [[Value::Int(1)], [Value::Int(3)]]);
    });
}

#[test]
fn deletes_all_rows_without_where() {
    with_dbs(|db| {
        seed(db);
        db.execute_sql("delete from student;").unwrap();
        let rs = db.execute_sql("select id from student;").unwrap();
        let (_, rows) = rows(&rs);
        assert_eq!(rows.len(), 0);
    });
}

#[test]
fn delete_errors() {
    with_dbs(|db| {
        seed(db);
        let err = db.execute_sql("delete from missing;").unwrap_err();
        assert!(err.to_string().contains("no such table"), "{err}");
        let err = db.execute_sql("delete from student where name;").unwrap_err();
        assert!(err.to_string().contains("boolean"), "{err}");
    });
}

#[test]
fn updates_matching_rows() {
    with_dbs(|db| {
        seed(db);
        db.execute_sql("update student set score = 100 where id = 2;").unwrap();
        let rs = db.execute_sql("select score from student where id = 2;").unwrap();
        let (_, rows) = rows(&rs);
        assert_eq!(rows, [[Value::Float(100.0)]]);
    });
}

#[test]
fn updates_with_row_expressions() {
    with_dbs(|db| {
        seed(db);
        db.execute_sql("update student set score = score + 1 where id <= 2;").unwrap();
        let rs = db.execute_sql("select score from student;").unwrap();
        let (_, rows) = rows(&rs);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], Value::Float(96.5));
        assert_eq!(rows[1][0], Value::Float(81.0));
        assert_eq!(rows[2][0], Value::Float(90.0), "unmatched row untouched");
    });
}

#[test]
fn updates_without_where_touches_all_rows() {
    with_dbs(|db| {
        seed(db);
        db.execute_sql("update student set id = id * 10;").unwrap();
        let rs = db.execute_sql("select id from student;").unwrap();
        let (_, rows) = rows(&rs);
        assert_eq!(rows, [[Value::Int(10)], [Value::Int(20)], [Value::Int(30)]]);
    });
}

#[test]
fn update_errors() {
    with_dbs(|db| {
        seed(db);
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
    });
}

#[test]
fn null_storage_and_filtering() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int, name char(10));").unwrap();
        db.execute_sql("insert into t values (1, null), (2, 'x');").unwrap();

        let rs = db.execute_sql("select name from t where id = 1;").unwrap();
        let (_, r) = rows(&rs);
        assert_eq!(r, [[Value::Null]]);

        let rs = db.execute_sql("select id from t where name is null;").unwrap();
        let (_, r) = rows(&rs);
        assert_eq!(r, [[Value::Int(1)]]);

        let rs = db.execute_sql("select id from t where name is not null;").unwrap();
        let (_, r) = rows(&rs);
        assert_eq!(r, [[Value::Int(2)]]);

        // NULL never satisfies = per SQL semantics
        let rs = db.execute_sql("select id from t where name = name;").unwrap();
        let (_, r) = rows(&rs);
        assert_eq!(r, [[Value::Int(2)]]);

        db.execute_sql("update t set name = 'y' where name is null;").unwrap();
        let rs = db.execute_sql("select name from t;").unwrap();
        let (_, r) = rows(&rs);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0][0], Value::Str("y".into()));
    });
}
