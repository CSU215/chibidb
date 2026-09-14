use chaoticdb::value::Value;
use chaoticdb::Database;

fn with_dbs(f: impl Fn(&Database)) {
    let mut mem = Database::open_in_memory().unwrap();
    f(&mut mem);

    let dir = tempfile::tempdir().unwrap();
    let mut file_db = Database::open(dir.path()).unwrap();
    f(&mut file_db);
}

fn seeded(db: &Database) {
    db.execute_sql("create table t (id int, name char(10), score float);").unwrap();
    db.execute_sql(
        "insert into t values (1, 'a', 80.0), (2, null, 90.5), (3, 'c', 90.5);",
    )
    .unwrap();
}

fn one_row(db: &Database, sql: &str) -> Vec<Value> {
    let rs = db.execute_sql(sql).unwrap();
    match &rs[0] {
        chaoticdb::ResultSet::Rows { rows, .. } => rows[0].clone(),
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

fn err(db: &Database, sql: &str) {
    assert!(db.execute_sql(sql).is_err(), "expected error for: {sql}");
}

#[test]
fn count_star_and_count_column() {
    with_dbs(|db| {
        seeded(db);
        assert_eq!(one_row(db, "select count(*) from t;"), [Value::Int(3)]);
        // nulls are not counted in count(col)
        assert_eq!(one_row(db, "select count(name) from t;"), [Value::Int(2)]);
        assert_eq!(one_row(db, "select count(id) from t;"), [Value::Int(3)]);
    });
}

#[test]
fn count_distinct() {
    with_dbs(|db| {
        seeded(db);
        assert_eq!(one_row(db, "select count(distinct id) from t;"), [Value::Int(3)]);
        assert_eq!(one_row(db, "select count(distinct name) from t;"), [Value::Int(2)]);
        assert_eq!(one_row(db, "select count(distinct score) from t;"), [Value::Int(2)]);
        // DISTINCT composes with the other aggregates
        assert_eq!(
            one_row(db, "select sum(distinct score) from t;"),
            [Value::Float(170.5)]
        );
    });
}

#[test]
fn count_empty_table_is_zero() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int);").unwrap();
        assert_eq!(one_row(db, "select count(*) from t;"), [Value::Int(0)]);
    });
}

#[test]
fn sum_avg_min_max() {
    with_dbs(|db| {
        seeded(db);
        assert_eq!(one_row(db, "select sum(id) from t;"), [Value::Int(6)]);
        assert_eq!(one_row(db, "select avg(id) from t;"), [Value::Float(2.0)]);
        assert_eq!(one_row(db, "select min(id) from t;"), [Value::Int(1)]);
        assert_eq!(one_row(db, "select max(id) from t;"), [Value::Int(3)]);
        // float sum stays float
        assert_eq!(one_row(db, "select sum(score) from t;"), [Value::Float(261.0)]);
        // min/max keep their element type
        assert_eq!(one_row(db, "select min(score) from t;"), [Value::Float(80.0)]);
    });
}

#[test]
fn aggregates_skip_nulls() {
    with_dbs(|db| {
        db.execute_sql("create table t (v int);").unwrap();
        db.execute_sql("insert into t values (1), (null), (3);").unwrap();
        assert_eq!(one_row(db, "select sum(v) from t;"), [Value::Int(4)]);
        assert_eq!(one_row(db, "select count(v) from t;"), [Value::Int(2)]);
        assert_eq!(one_row(db, "select avg(v) from t;"), [Value::Float(2.0)]);
    });
}

#[test]
fn aggregates_over_empty_set() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int, v float);").unwrap();
        assert_eq!(one_row(db, "select count(*) from t;"), [Value::Int(0)]);
        assert_eq!(one_row(db, "select count(id) from t;"), [Value::Int(0)]);
        assert_eq!(one_row(db, "select sum(id) from t;"), [Value::Null]);
        assert_eq!(one_row(db, "select avg(v) from t;"), [Value::Null]);
        assert_eq!(one_row(db, "select min(id) from t;"), [Value::Null]);
        assert_eq!(one_row(db, "select max(v) from t;"), [Value::Null]);
    });
}

#[test]
fn aggregates_combine_with_where() {
    with_dbs(|db| {
        seeded(db);
        assert_eq!(one_row(db, "select count(*) from t where id >= 2;"), [Value::Int(2)]);
        assert_eq!(one_row(db, "select max(score) from t where id = 1;"), [Value::Float(80.0)]);
    });
}

#[test]
fn multiple_aggregates_and_arithmetic() {
    with_dbs(|db| {
        seeded(db);
        let row = one_row(db, "select count(*), sum(id) + 1, min(id) * 10 from t;");
        assert_eq!(row, [Value::Int(3), Value::Int(7), Value::Int(10)]);
    });
}

#[test]
fn aggregate_errors() {
    with_dbs(|db| {
        seeded(db);
        // bare column next to aggregate without group by
        err(db, "select id, count(*) from t;");
        // aggregate inside where
        err(db, "select id from t where count(*) = 1;");
        // aggregate inside insert values
        err(db, "insert into t values (count(*));");
        // sum over strings
        err(db, "select sum(name) from t;");
        // avg over strings
        err(db, "select avg(name) from t;");
        // aggregate in group by
        err(db, "select name, count(*) from t group by count(*);");
    });
}

#[test]
fn group_by_counts_per_group() {
    with_dbs(|db| {
        db.execute_sql("create table t (dept char(4), score int);").unwrap();
        db.execute_sql(
            "insert into t values ('a', 1), ('b', 2), ('a', 3), ('c', 4), ('b', 5);",
        )
        .unwrap();

        let rs = db
            .execute_sql("select dept, count(*), sum(score) from t group by dept;")
            .unwrap();
        match &rs[0] {
            chaoticdb::ResultSet::Rows { columns, rows } => {
                assert_eq!(columns.as_slice(), ["dept", "(count *)", "(sum score)"]);
                assert_eq!(rows.len(), 3);
                assert_eq!(rows[0], [Value::Str("a".into()), Value::Int(2), Value::Int(4)]);
                assert_eq!(rows[1], [Value::Str("b".into()), Value::Int(2), Value::Int(7)]);
                assert_eq!(rows[2], [Value::Str("c".into()), Value::Int(1), Value::Int(4)]);
            }
            other => panic!("expected rows, got {other:?}"),
        }
    });
}

#[test]
fn group_by_with_where_and_having() {
    with_dbs(|db| {
        db.execute_sql("create table t (dept char(4), score int);").unwrap();
        db.execute_sql(
            "insert into t values ('a', 1), ('b', 2), ('a', 3), ('c', 4), ('b', 5), ('a', 10);",
        )
        .unwrap();

        let rs = db
            .execute_sql(
                "select dept, count(*) from t where score < 10 group by dept having count(*) > 1;",
            )
            .unwrap();
        match &rs[0] {
            chaoticdb::ResultSet::Rows { rows, .. } => {
                // group a: rows (1,3) -> count 2; b: (2,5) -> 2; c: (4) -> 1
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0][0], Value::Str("a".into()));
                assert_eq!(rows[0][1], Value::Int(2));
                assert_eq!(rows[1][0], Value::Str("b".into()));
            }
            other => panic!("expected rows, got {other:?}"),
        }
    });
}

#[test]
fn group_by_multiple_columns_and_empty_groups() {
    with_dbs(|db| {
        db.execute_sql("create table t (a int, b int, v int);").unwrap();
        db.execute_sql("insert into t values (1, 1, 10), (1, 2, 20), (1, 1, 30);").unwrap();

        let rs = db
            .execute_sql("select a, b, count(*) from t group by a, b;")
            .unwrap();
        match &rs[0] {
            chaoticdb::ResultSet::Rows { rows, .. } => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0], [Value::Int(1), Value::Int(1), Value::Int(2)]);
                assert_eq!(rows[1], [Value::Int(1), Value::Int(2), Value::Int(1)]);
            }
            other => panic!("expected rows, got {other:?}"),
        }

        // where filters everything -> no groups -> no rows
        let rs = db
            .execute_sql("select a, count(*) from t where a > 99 group by a;")
            .unwrap();
        match &rs[0] {
            chaoticdb::ResultSet::Rows { rows, .. } => assert_eq!(rows.len(), 0),
            other => panic!("expected rows, got {other:?}"),
        }
    });
}

#[test]
fn order_by_sorts_rows() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int, score float);").unwrap();
        db.execute_sql(
            "insert into t values (3, 30.0), (1, 20.0), (2, 10.0), (4, 20.0);",
        )
        .unwrap();

        let col = |sql: &str, db: &Database| -> Vec<Value> {
            let rs = db.execute_sql(sql).unwrap();
            match &rs[0] {
                chaoticdb::ResultSet::Rows { rows, .. } => rows.iter().map(|r| r[0].clone()).collect(),
                other => panic!("expected rows for {sql}, got {other:?}"),
            }
        };

        assert_eq!(
            col("select id from t order by score;", db),
            [Value::Int(2), Value::Int(1), Value::Int(4), Value::Int(3)],
            "stable sort keeps insertion order for ties"
        );
        assert_eq!(
            col("select id from t order by score desc;", db),
            [Value::Int(3), Value::Int(1), Value::Int(4), Value::Int(2)]
        );
        assert_eq!(
            col("select id from t order by score asc, id desc;", db),
            [Value::Int(2), Value::Int(4), Value::Int(1), Value::Int(3)]
        );
        assert_eq!(
            col("select id from t order by score * -1;", db),
            [Value::Int(3), Value::Int(1), Value::Int(4), Value::Int(2)]
        );
    });
}

#[test]
fn order_by_nulls_sort_first_on_asc() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int);").unwrap();
        db.execute_sql("insert into t values (2), (null), (1);").unwrap();
        let rs = db.execute_sql("select id from t order by id;").unwrap();
        match &rs[0] {
            chaoticdb::ResultSet::Rows { rows, .. } => {
                assert_eq!(
                    rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
                    [Value::Null, Value::Int(1), Value::Int(2)]
                );
            }
            other => panic!("expected rows, got {other:?}"),
        }
    });
}

#[test]
fn order_by_aggregate_output() {
    with_dbs(|db| {
        db.execute_sql("create table t (dept char(4), score int);").unwrap();
        db.execute_sql("insert into t values ('a', 1), ('b', 2), ('a', 3), ('c', 4);")
            .unwrap();
        let rs = db
            .execute_sql(
                "select dept, count(*) from t group by dept order by count(*) desc, dept;",
            )
            .unwrap();
        match &rs[0] {
            chaoticdb::ResultSet::Rows { rows, .. } => {
                assert_eq!(rows.len(), 3);
                assert_eq!(rows[0][0], Value::Str("a".into()));
                assert_eq!(rows[0][1], Value::Int(2));
                assert_eq!(rows[1][0], Value::Str("b".into()));
                assert_eq!(rows[2][0], Value::Str("c".into()));
            }
            other => panic!("expected rows, got {other:?}"),
        }
    });
}

#[test]
fn limit_and_offset() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int);").unwrap();
        for i in 0..5 {
            db.execute_sql(&format!("insert into t values ({i});")).unwrap();
        }
        let ids = |sql: &str, db: &Database| -> Vec<i64> {
            let rs = db.execute_sql(sql).unwrap();
            match &rs[0] {
                chaoticdb::ResultSet::Rows { rows, .. } => rows
                    .iter()
                    .map(|r| match &r[0] {
                        Value::Int(n) => *n,
                        other => panic!("unexpected {other:?}"),
                    })
                    .collect(),
                other => panic!("expected rows for {sql}, got {other:?}"),
            }
        };

        assert_eq!(ids("select id from t limit 2;", db), [0, 1]);
        assert_eq!(ids("select id from t limit 2 offset 1;", db), [1, 2]);
        assert_eq!(ids("select id from t limit 0;", db), []);
        assert_eq!(ids("select id from t limit 100;", db), [0, 1, 2, 3, 4]);
        assert_eq!(ids("select id from t limit 100 offset 3;", db), [3, 4]);
        assert_eq!(
            ids("select id from t order by id desc limit 2;", db),
            [4, 3],
            "limit applies after order by"
        );
        assert_eq!(
            ids("select id, count(*) from t group by id order by id desc limit 2;", db),
            [4, 3],
            "limit applies to grouped output"
        );
        err(db, "select id from t limit -1;");
        err(db, "select id from t limit 'a';");
    });
}

#[test]
fn having_without_aggregate_still_works() {
    with_dbs(|db| {
        db.execute_sql("create table t (dept char(4), score int);").unwrap();
        db.execute_sql("insert into t values ('a', 1), ('b', 2), ('a', 3);").unwrap();
        let rs = db
            .execute_sql("select dept, count(*) from t group by dept having dept = 'b';")
            .unwrap();
        match &rs[0] {
            chaoticdb::ResultSet::Rows { rows, .. } => {
                assert_eq!(rows.as_slice(), [[Value::Str("b".into()), Value::Int(1)]]);
            }
            other => panic!("expected rows, got {other:?}"),
        }
    });
}
