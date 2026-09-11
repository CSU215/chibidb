//! M22: UNION / UNION ALL.

use chibidb::value::Value;
use chibidb::{Database, ResultSet};

fn seed(db: &Database) {
    db.execute_sql("create table t1 (a int);").unwrap();
    db.execute_sql("insert into t1 values (1), (2), (2), (3);").unwrap();
    db.execute_sql("create table t2 (b int);").unwrap();
    db.execute_sql("insert into t2 values (2), (3), (4);").unwrap();
}

fn q(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    let rs = db.execute_sql(sql).unwrap();
    assert_eq!(rs.len(), 1, "sql: {sql}");
    match &rs[0] {
        ResultSet::Rows { rows, .. } => rows.clone(),
        other => panic!("expected rows for {sql}, got {other:?}"),
    }
}

#[test]
fn union_all_concatenates_and_union_dedups() {
    let db = Database::open_in_memory().unwrap();
    seed(&db);

    assert_eq!(
        q(&db, "select a from t1 union all select b from t2;"),
        [
            [Value::Int(1)],
            [Value::Int(2)],
            [Value::Int(2)],
            [Value::Int(3)],
            [Value::Int(2)],
            [Value::Int(3)],
            [Value::Int(4)],
        ]
    );
    assert_eq!(
        q(&db, "select a from t1 union select b from t2;"),
        [[Value::Int(1)], [Value::Int(2)], [Value::Int(3)], [Value::Int(4)]]
    );
}

#[test]
fn union_takes_left_column_names_and_order_by_limit_apply_to_whole() {
    let db = Database::open_in_memory().unwrap();
    seed(&db);

    let rs = db
        .execute_sql("select a from t1 union select b from t2 order by a desc limit 2;")
        .unwrap();
    match &rs[0] {
        ResultSet::Rows { columns, rows } => {
            assert_eq!(columns, &["a".to_string()]);
            assert_eq!(rows.to_vec(), [[Value::Int(4)], [Value::Int(3)]]);
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn union_chains_and_mixes_all() {
    let db = Database::open_in_memory().unwrap();
    seed(&db);
    assert_eq!(
        q(
            &db,
            "select a from t1 union select b from t2 union all select a from t1;"
        ),
        [
            [Value::Int(1)],
            [Value::Int(2)],
            [Value::Int(3)],
            [Value::Int(4)],
            [Value::Int(1)],
            [Value::Int(2)],
            [Value::Int(2)],
            [Value::Int(3)],
        ]
    );
}

#[test]
fn union_column_count_must_match() {
    let db = Database::open_in_memory().unwrap();
    seed(&db);
    let err = db.execute_sql("select a from t1 union select b, 1 from t2;").unwrap_err();
    assert!(err.to_string().contains("column count"), "{err}");
}
