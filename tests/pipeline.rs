use chaoticdb::{Database, ResultSet, Value};

fn message(rs: &[ResultSet]) -> String {
    match &rs[0] {
        ResultSet::Message(m) => m.clone(),
        other => panic!("expected message, got {other:?}"),
    }
}

#[test]
fn select_from_missing_table_errors() {
    let db = Database::open_in_memory().unwrap();
    let err = db.execute_sql("select * from missing;").unwrap_err();
    assert!(err.to_string().contains("no such table"), "{err}");
}

#[test]
fn indexed_select_reports_an_index_scan_plan() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int);").unwrap();
    db.execute_sql("create index idx on t (id);").unwrap();

    let plan = message(&db.execute_sql("explain select * from t where id = 1;").unwrap());
    assert!(plan.contains("IndexScan"), "{plan}");
}

#[test]
fn plain_select_returns_rows() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int);").unwrap();
    db.execute_sql("insert into t values (1);").unwrap();

    let out = db.execute_sql("select id from t;").unwrap();
    match &out[0] {
        ResultSet::Rows { rows, .. } => assert_eq!(rows, &vec![vec![Value::Int(1)]]),
        other => panic!("expected rows, got {other:?}"),
    }
}
