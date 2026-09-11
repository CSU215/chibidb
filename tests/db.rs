use chibidb::value::Value;
use chibidb::{Database, ResultSet};

fn rows(rs: &[ResultSet]) -> (&[String], &[Vec<Value>]) {
    match &rs[0] {
        ResultSet::Rows { columns, rows } => (columns, rows),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn message(rs: &[ResultSet]) -> String {
    match &rs[0] {
        ResultSet::Message(m) => m.clone(),
        other => panic!("expected message, got {other:?}"),
    }
}

/// Runs `f` against every backend: in-memory and file-backed.
fn with_dbs(f: impl Fn(&Database)) {
    let mut mem = Database::open_in_memory().unwrap();
    f(&mut mem);

    let dir = tempfile::tempdir().unwrap();
    let mut file_db = Database::open(dir.path()).unwrap();
    f(&mut file_db);
}

fn seed(db: &Database) {
    db.execute_sql("create table student (id int, name char(10), score float);")
        .unwrap();
    db.execute_sql(
        "insert into student values (1, 'alice', 95.5), (2, 'bob', 80), (3, 'carol', 90);",
    )
    .unwrap();
}

#[test]
fn executes_constant_select() {
    let db = Database::open_in_memory().unwrap();
    let rs = db.execute_sql("select 1+2, 'ab';").unwrap();
    assert_eq!(rs.len(), 1);
    let (columns, rows) = rows(&rs);
    assert_eq!(columns, ["(+ 1 2)", "'ab'"]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], [Value::Int(3), Value::Str("ab".into())]);
}

#[test]
fn executes_each_statement() {
    let db = Database::open_in_memory().unwrap();
    let rs = db.execute_sql("select 1; select 2;").unwrap();
    assert_eq!(rs.len(), 2);
    let (_, r1) = rows(&rs[0..1]);
    assert_eq!(r1[0][0], Value::Int(1));
    let (_, r2) = rows(&rs[1..2]);
    assert_eq!(r2[0][0], Value::Int(2));
}

#[test]
fn empty_sql_yields_no_results() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(db.execute_sql("").unwrap().len(), 0);
    assert_eq!(db.execute_sql(";;").unwrap().len(), 0);
}

#[test]
fn star_requires_from() {
    let db = Database::open_in_memory().unwrap();
    assert!(db.execute_sql("select *;").is_err());
}

#[test]
fn runtime_errors_propagate() {
    let db = Database::open_in_memory().unwrap();
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
fn filters_with_like() {
    with_dbs(|db| {
        seed(db);
        let rs = db.execute_sql("select name from student where name like 'a%';").unwrap();
        let (_, starts_with_a) = rows(&rs);
        assert_eq!(starts_with_a, [[Value::Str("alice".into())]]);

        let rs = db
            .execute_sql("select name from student where name not like '_o%';")
            .unwrap();
        let (_, matched) = rows(&rs);
        assert_eq!(matched, [[Value::Str("alice".into())], [Value::Str("carol".into())]]);
    });
}

#[test]
fn filters_with_modulo() {
    with_dbs(|db| {
        seed(db);
        let rs = db.execute_sql("select id from student where id % 2 = 1;").unwrap();
        let (_, rows) = rows(&rs);
        assert_eq!(rows, [[Value::Int(1)], [Value::Int(3)]]);
    });
}

#[test]
fn selects_with_string_functions() {
    with_dbs(|db| {
        seed(db);
        let rs = db.execute_sql("select upper(name) from student where id = 1;").unwrap();
        let (_, upper) = rows(&rs);
        assert_eq!(upper, [[Value::Str("ALICE".into())]]);

        let rs = db.execute_sql("select name from student where length(name) = 3;").unwrap();
        let (_, matched) = rows(&rs);
        assert_eq!(matched, [[Value::Str("bob".into())]]);
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
        let rs = db.execute_sql("select score from student order by id;").unwrap();
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
fn null_storage_and_filtering() {    with_dbs(|db| {
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
        let rs = db.execute_sql("select name from t order by id;").unwrap();
        let (_, r) = rows(&rs);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0][0], Value::Str("y".into()));
    });
}

#[test]
fn text_type() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int, body text);").unwrap();
        let long = "x".repeat(5000);
        db.execute_sql(&format!(
            "insert into t values (1, '{long}'), (2, 'short');"
        ))
        .unwrap();

        let rs = db.execute_sql("select body from t where id = 1;").unwrap();
        let (_, r) = rows(&rs);
        assert_eq!(r[0][0], Value::Str(long.clone()));

        // text longer than a page cannot be stored
        let huge = "y".repeat(20000);
        let err = db.execute_sql(&format!("insert into t values (3, '{huge}');")).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");

        // numbers do not fit into text columns
        let err = db.execute_sql("insert into t values (4, 123);").unwrap_err();
        assert!(err.to_string().contains("cannot insert"), "{err}");

        db.execute_sql("update t set body = null where id = 2;").unwrap();
        let rs = db.execute_sql("select body from t where id = 2;").unwrap();
        let (_, r) = rows(&rs);
        assert_eq!(r, [[Value::Null]]);
    });
}

#[test]
fn in_list_filters_with_three_valued_logic() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int, name char(4));").unwrap();
        db.execute_sql("insert into t values (1, 'a'), (2, 'b'), (3, 'c'), (null, 'n');")
            .unwrap();
        let ids = |db: &Database, sql: &str| -> Vec<i64> {
            let rs = db.execute_sql(sql).unwrap();
            let (_, r) = rows(&rs);
            r.iter()
                .map(|row| match row[0] {
                    Value::Int(n) => n,
                    ref v => panic!("{v:?}"),
                })
                .collect()
        };

        assert_eq!(ids(db, "select id from t where id in (1, 3) order by id;"), [1, 3]);
        assert_eq!(ids(db, "select id from t where id not in (1, 3) order by id;"), [2]);
        // NULL comparisons are UNKNOWN: excluded from IN results
        assert_eq!(ids(db, "select id from t where id in (1, null);"), [1]);
        // NOT IN over a list containing NULL can never be TRUE
        assert_eq!(ids(db, "select id from t where id not in (1, null);"), Vec::<i64>::new());
        // the literal NULL row never matches either direction
        assert_eq!(ids(db, "select id from t where id not in (1, 3);"), [2]);
    });
}

#[test]
fn in_subquery_matches_uncorrelated_selects() {
    with_dbs(|db| {
        db.execute_sql("create table a (id int);").unwrap();
        db.execute_sql("create table b (v int, a_id int);").unwrap();
        db.execute_sql("insert into a values (1), (2), (3), (null);").unwrap();
        db.execute_sql("insert into b values (10, 1), (20, 2), (30, 3), (40, null);")
            .unwrap();

        let vs = |db: &Database, sql: &str| -> Vec<i64> {
            let rs = db.execute_sql(sql).unwrap();
            let (_, r) = rows(&rs);
            r.iter()
                .map(|row| match row[0] {
                    Value::Int(n) => n,
                    ref v => panic!("{v:?}"),
                })
                .collect()
        };

        // the subquery's NULL row must not match anything (UNKNOWN)
        assert_eq!(
            vs(db, "select v from b where a_id in (select id from a) order by v;"),
            [10, 20, 30]
        );
        assert_eq!(
            vs(db, "select v from b where a_id not in (select id from a) order by v;"),
            Vec::<i64>::new(),
            "NOT IN over a set containing NULL yields no rows"
        );
        // empty subquery: IN matches nothing, NOT IN matches everything
        assert_eq!(
            vs(db, "select v from b where a_id in (select id from a where id > 99);"),
            Vec::<i64>::new()
        );
        assert_eq!(
            vs(db, "select v from b where a_id not in (select id from a where id > 99) order by v;"),
            [10, 20, 30, 40]
        );
        // subquery with aggregates and where clauses works too
        assert_eq!(
            vs(db, "select v from b where a_id in (select max(id) from a);"),
            [30]
        );
        // multi-column subquery is rejected
        let err = db.execute_sql("select v from b where a_id in (select id, a_id from a, b);").unwrap_err();
        assert!(err.to_string().contains("single column"), "{err}");
    });
}

#[test]
fn exists_predicate() {
    with_dbs(|db| {
        db.execute_sql("create table a (id int);").unwrap();
        db.execute_sql("create table b (v int);").unwrap();
        db.execute_sql("insert into a values (1);").unwrap();
        db.execute_sql("insert into b values (10), (20);").unwrap();

        // uncorrelated EXISTS: every row of b sees a non-empty a
        let rs = db.execute_sql("select v from b where exists (select id from a);").unwrap();
        assert_eq!(rows(&rs).1.len(), 2);
        // NOT EXISTS folds through the existing unary NOT
        let rs = db.execute_sql("select v from b where not exists (select id from a);").unwrap();
        assert_eq!(rows(&rs).1.len(), 0);
        let rs = db
            .execute_sql("select v from b where exists (select id from a where id > 99);")
            .unwrap();
        assert_eq!(rows(&rs).1.len(), 0);
    });
}

#[test]
fn update_delete_with_subqueries() {
    with_dbs(|db| {
        db.execute_sql("create table a (id int);").unwrap();
        db.execute_sql("insert into a values (1), (2);").unwrap();
        db.execute_sql("create table b (v int);").unwrap();
        db.execute_sql("insert into b values (1), (20), (30);").unwrap();

        // DELETE ... WHERE <subquery>
        db.execute_sql("delete from b where v in (select id from a);").unwrap();
        assert_eq!(
            query_ids(db, "select v from b order by v;"),
            [20, 30]
        );
        // uncorrelated EXISTS
        db.execute_sql("delete from b where exists (select 1 from a where id > 100);")
            .unwrap();
        assert_eq!(query_ids(db, "select v from b order by v;"), [20, 30]);

        // UPDATE ... SET <scalar subquery>
        db.execute_sql("update b set v = (select max(id) from a);").unwrap();
        assert_eq!(query_ids(db, "select v from b order by v;"), [2, 2]);
        // UPDATE ... WHERE <subquery>
        db.execute_sql("update b set v = 99 where v = (select max(id) from a);").unwrap();
        assert_eq!(query_ids(db, "select v from b order by v;"), [99, 99]);
    });
}

fn query_ids(db: &Database, sql: &str) -> Vec<i64> {
    let rs = db.execute_sql(sql).unwrap();
    let (_, rows) = rows(&rs);
    rows.iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            ref v => panic!("{v:?}"),
        })
        .collect()
}

#[test]
fn scalar_subquery() {
    with_dbs(|db| {
        db.execute_sql("create table a (id int);").unwrap();
        db.execute_sql("create table b (v int);").unwrap();
        db.execute_sql("insert into a values (7), (9);").unwrap();
        db.execute_sql("insert into b values (7), (9), (100);").unwrap();

        let rs = db.execute_sql("select v from b where v = (select max(id) from a);").unwrap();
        assert_eq!(rows(&rs).1, [[Value::Int(9)]]);

        // arithmetic over a scalar subquery
        let rs = db
            .execute_sql("select v from b where v = (select max(id) from a) - 2;")
            .unwrap();
        assert_eq!(rows(&rs).1, [[Value::Int(7)]]);

        // empty scalar subquery yields NULL, which compares UNKNOWN
        let rs = db
            .execute_sql("select v from b where v = (select id from a where id > 99);")
            .unwrap();
        assert_eq!(rows(&rs).1.len(), 0);

        // more than one row is an error
        let err = db.execute_sql("select v from b where v = (select id from a);").unwrap_err();
        assert!(err.to_string().contains("more than one row"), "{err}");
    });
}

#[test]
fn create_view_select_drop_view() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int, score float);").unwrap();
        db.execute_sql("insert into t values (1, 55.0), (2, 75.0), (3, 95.0);").unwrap();

        let m = message(&db.execute_sql("create view passed as select id, score from t where score >= 75.0;").unwrap());
        assert_eq!(m, "SUCCESS");

        // query through the view
        let rs = db.execute_sql("select * from passed order by id;").unwrap();
        let (cols, r) = rows(&rs);
        assert_eq!(cols, ["id", "score"]);
        assert_eq!(r, [[Value::Int(2), Value::Float(75.0)], [Value::Int(3), Value::Float(95.0)]]);

        // filtering on the view
        let rs = db.execute_sql("select id from passed where score > 80.0;").unwrap();
        assert_eq!(rows(&rs).1, [[Value::Int(3)]]);

        // aggregates over views
        let rs = db.execute_sql("select count(*) from passed;").unwrap();
        assert_eq!(rows(&rs).1, [[Value::Int(2)]]);

        // joins between views and tables
        let rs = db.execute_sql("select t.id from t join passed on t.id = passed.id order by t.id;").unwrap();
        assert_eq!(rows(&rs).1, [[Value::Int(2)], [Value::Int(3)]]);

        // name collisions
        let err = db.execute_sql("create view passed as select 1;").unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        let err = db.execute_sql("create table passed (x int);").unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");

        // drop and recreate
        assert_eq!(message(&db.execute_sql("drop view passed;").unwrap()), "SUCCESS");
        let err = db.execute_sql("select * from passed;").unwrap_err();
        assert!(err.to_string().contains("no such table"), "{err}");
        let err = db.execute_sql("drop view passed;").unwrap_err();
        assert!(err.to_string().contains("no such view"), "{err}");
        db.execute_sql("create view passed as select id from t where id = 1;").unwrap();
        assert_eq!(rows(&db.execute_sql("select * from passed;").unwrap()).1, [[Value::Int(1)]]);
    });
}

#[test]
fn view_validation_and_persistence() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int);").unwrap();
        db.execute_sql("insert into t values (1), (2);").unwrap();

        // view over a missing table is rejected at creation
        let err = db.execute_sql("create view v as select * from missing;").unwrap_err();
        assert!(err.to_string().contains("no such table"), "{err}");
        // self-reference is rejected (the view does not exist yet)
        let err = db.execute_sql("create view v as select * from v;").unwrap_err();
        assert!(err.to_string().contains("no such table"), "{err}");

        db.execute_sql("create view v as select id from t where id > 1;").unwrap();
    }
    let db = Database::open(dir.path()).unwrap();
    let rs = db.execute_sql("select * from v;").unwrap();
    assert_eq!(rows(&rs).1, [[Value::Int(2)]]);
}

#[test]
fn distinct_deduplicates_projected_rows() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int, name char(8));").unwrap();
        db.execute_sql(
            "insert into t values (1, 'a'), (2, 'a'), (3, 'b'), (4, null), (5, null);",
        )
        .unwrap();

        // NULLs count as duplicates of each other (SQL semantics)
        let rs = db.execute_sql("select distinct name from t order by name;").unwrap();
        assert_eq!(
            rows(&rs).1,
            [[Value::Null], [Value::Str("a".into())], [Value::Str("b".into())]]
        );

        // distinct over an expression (integer division truncates)
        let rs = db.execute_sql("select distinct id / 2 from t order by id / 2;").unwrap();
        assert_eq!(rows(&rs).1, [[Value::Int(0)], [Value::Int(1)], [Value::Int(2)]]);

        // multi-column distinct pairs
        db.execute_sql("create table p (a int, b int);").unwrap();
        db.execute_sql("insert into p values (1, 1), (1, 2), (1, 1), (2, 1);").unwrap();
        let rs = db.execute_sql("select distinct a, b from p order by a, b;").unwrap();
        assert_eq!(
            rows(&rs).1,
            [[Value::Int(1), Value::Int(1)], [Value::Int(1), Value::Int(2)], [Value::Int(2), Value::Int(1)]]
        );

        // distinct then limit
        let rs = db.execute_sql("select distinct a from p limit 1;").unwrap();
        assert_eq!(rows(&rs).1, [[Value::Int(1)]]);
    });
}

#[test]
fn left_join_keeps_unmatched_left_rows() {
    with_dbs(|db| {
        db.execute_sql("create table a (id int);").unwrap();
        db.execute_sql("create table b (a_id int, v char(4));").unwrap();
        db.execute_sql("insert into a values (1), (2), (3);").unwrap();
        db.execute_sql("insert into b values (1, 'x'), (3, 'y');").unwrap();

        let rs = db
            .execute_sql("select a.id, b.v from a left join b on a.id = b.a_id order by a.id;")
            .unwrap();
        assert_eq!(
            rows(&rs).1,
            [
                [Value::Int(1), Value::Str("x".into())],
                [Value::Int(2), Value::Null],
                [Value::Int(3), Value::Str("y".into())],
            ]
        );

        // inner join only keeps matches
        let rs = db
            .execute_sql("select a.id, b.v from a join b on a.id = b.a_id order by a.id;")
            .unwrap();
        assert_eq!(
            rows(&rs).1,
            [[Value::Int(1), Value::Str("x".into())], [Value::Int(3), Value::Str("y".into())]]
        );

        // where over the left-join result
        let rs = db
            .execute_sql(
                "select a.id, b.v from a left join b on a.id = b.a_id where b.v is null;",
            )
            .unwrap();
        assert_eq!(rows(&rs).1, [[Value::Int(2), Value::Null]]);

        // chained left joins
        db.execute_sql("create table c (a_id int, w char(4));").unwrap();
        db.execute_sql("insert into c values (1, 'z');").unwrap();
        let rs = db
            .execute_sql(
                "select a.id, b.v, c.w from a left join b on a.id = b.a_id
                 left join c on a.id = c.a_id order by a.id;",
            )
            .unwrap();
        assert_eq!(
            rows(&rs).1,
            [
                [Value::Int(1), Value::Str("x".into()), Value::Str("z".into())],
                [Value::Int(2), Value::Null, Value::Null],
                [Value::Int(3), Value::Str("y".into()), Value::Null],
            ]
        );

        // empty right side: every left row survives with NULLs
        db.execute_sql("create table e (a_id int);").unwrap();
        let rs = db
            .execute_sql("select a.id, e.a_id from a left join e on a.id = e.a_id order by a.id;")
            .unwrap();
        assert_eq!(
            rows(&rs).1,
            [[Value::Int(1), Value::Null], [Value::Int(2), Value::Null], [Value::Int(3), Value::Null]]
        );
    });
}

#[test]
fn date_type() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int, birthday date);").unwrap();
        db.execute_sql("insert into t values (1, '2000-02-29'), (2, '1999-06-15');")
            .unwrap();

        let err = db.execute_sql("insert into t values (3, '2023-02-29');").unwrap_err();
        assert!(err.to_string().contains("invalid date"), "{err}");

        let rs = db.execute_sql("select birthday from t where id = 2;").unwrap();
        let (_, r) = rows(&rs);
        assert_eq!(r, [[Value::Date(10757)]], "1999-06-15");
        assert_eq!(r[0][0].to_string(), "1999-06-15");

        let rs =
            db.execute_sql("select id from t where birthday = '2000-02-29';").unwrap();
        let (_, r) = rows(&rs);
        assert_eq!(r, [[Value::Int(1)]]);

        let rs = db.execute_sql("select id from t where birthday > '2000-01-01';").unwrap();
        let (_, r) = rows(&rs);
        assert_eq!(r, [[Value::Int(1)]]);

        let err = db.execute_sql("insert into t values (4, 'not a date');").unwrap_err();
        assert!(err.to_string().contains("invalid date"), "{err}");
    });
}

#[test]
fn drop_table_removes_schema_data_and_files() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    db.execute_sql("create table t (id int, name char(8));").unwrap();
    db.execute_sql("create index idx on t (id);").unwrap();
    db.execute_sql("insert into t values (1, 'a');").unwrap();

    let m = message(&db.execute_sql("drop table t;").unwrap());
    assert_eq!(m, "SUCCESS");

    let err = db.execute_sql("select * from t;").unwrap_err();
    assert!(err.to_string().contains("no such table"), "{err}");
    let err = db.execute_sql("insert into t values (2, 'b');").unwrap_err();
    assert!(err.to_string().contains("no such table"), "{err}");
    let err = db.execute_sql("drop table t;").unwrap_err();
    assert!(err.to_string().contains("no such table"), "{err}");

    // the table name and index name are free again
    db.execute_sql("create table t (id int);").unwrap();
    db.execute_sql("create index idx on t (id);").unwrap();

    // physical files of the dropped table were removed
    assert_eq!(std::fs::read_dir(dir.path().join("tables")).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(dir.path().join("indexes")).unwrap().count(), 1);

    // and the state is stable across a reopen
    drop(db);
    let db = Database::open(dir.path()).unwrap();
    db.execute_sql("insert into t values (7);").unwrap();
    let rs = db.execute_sql("select id from t;").unwrap();
    assert_eq!(rows(&rs).1, [[Value::Int(7)]]);
}

#[test]
fn drop_table_ddl_validation() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int);").unwrap();
        let mut session = chibidb::Session::new();
        db.execute_sql_with(&mut session, "begin;").unwrap();
        let err = db.execute_sql_with(&mut session, "drop table t;").unwrap_err();
        assert!(err.to_string().contains("DDL inside a transaction"), "{err}");
        db.execute_sql_with(&mut session, "rollback;").unwrap();
        let rs = db.execute_sql_with(&mut session, "drop table t;").unwrap();
        assert_eq!(message(&rs), "SUCCESS");
    });
}
