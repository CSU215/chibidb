use chaoticdb::value::Value;
use chaoticdb::{Database, ResultSet};

fn count_with(dir: &std::path::Path, suffix: &str) -> usize {
    std::fs::read_dir(dir.join("tables"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(suffix)
        })
        .count()
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute_sql(sql).unwrap().remove(0) {
        ResultSet::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn engine_clause_selects_storage() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // the default engine is heap
    db.execute_sql("create table h (id int);").unwrap();
    db.execute_sql("create table l (id int) engine = lsm;").unwrap();
    db.execute_sql("insert into h values (1);").unwrap();
    db.execute_sql("insert into l values (2);").unwrap();

    assert_eq!(count_with(dir.path(), ".dbf"), 1);
    assert_eq!(count_with(dir.path(), ".lsm"), 1);
    assert_eq!(rows(&db, "select id from h;"), [[Value::Int(1)]]);
    assert_eq!(rows(&db, "select id from l;"), [[Value::Int(2)]]);
}

#[test]
fn engine_clause_is_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    db.execute_sql("create table t (id int) ENGINE = LSM;").unwrap();
    assert_eq!(count_with(dir.path(), ".lsm"), 1);
}

#[test]
fn unknown_engine_is_rejected() {
    let db = Database::open_in_memory().unwrap();
    let err = db.execute_sql("create table t (id int) engine = btree;").unwrap_err();
    assert!(err.to_string().contains("engine name"), "{err}");
}
