use chibidb::value::Value;
use chibidb::Database;

fn with_dbs(f: impl Fn(&mut Database)) {
    let mut mem = Database::open_in_memory().unwrap();
    f(&mut mem);

    let dir = tempfile::tempdir().unwrap();
    let mut file_db = Database::open(dir.path()).unwrap();
    f(&mut file_db);
}

fn seeded(db: &mut Database) {
    db.execute_sql("create table t (id int, name char(10), score float);").unwrap();
    db.execute_sql(
        "insert into t values (1, 'a', 80.0), (2, null, 90.5), (3, 'c', 90.5);",
    )
    .unwrap();
}

fn one_row(db: &mut Database, sql: &str) -> Vec<Value> {
    let rs = db.execute_sql(sql).unwrap();
    match &rs[0] {
        chibidb::ResultSet::Rows { rows, .. } => rows[0].clone(),
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

fn err(db: &mut Database, sql: &str) {
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
    });
}
