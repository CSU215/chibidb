use chaoticdb::config::Config;
use chaoticdb::value::Value;
use chaoticdb::{Database, ResultSet};

fn config() -> Config {
    Config::from_toml_str("[storage]\ndouble_write = true\n").unwrap()
}

fn count(db: &Database) -> i64 {
    let rs = db.execute_sql("select count(*) from t;").unwrap();
    match &rs[0] {
        ResultSet::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            ref v => panic!("expected int, got {v:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn double_write_flushes_and_reopens_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open_with_config(dir.path(), &config()).unwrap();
        db.execute_sql("create table t (id int);").unwrap();
        db.execute_sql("insert into t values (1), (2);").unwrap();
        db.flush().unwrap(); // checkpoint goes through the double-write buffer
    }

    let db = Database::open_with_config(dir.path(), &config()).unwrap();
    assert!(db.config().storage.double_write);
    assert_eq!(count(&db), 2);
    // a clean flush truncates the buffer
    assert_eq!(std::fs::metadata(dir.path().join("dwb.bin")).unwrap().len(), 0);
}

#[test]
fn double_write_leaves_no_residue_across_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with_config(dir.path(), &config()).unwrap();
    db.execute_sql("create table t (id int);").unwrap();
    db.execute_sql("insert into t values (1);").unwrap();
    drop(db);

    let db = Database::open_with_config(dir.path(), &config()).unwrap();
    db.execute_sql("insert into t values (2);").unwrap();
    drop(db);

    let db = Database::open_with_config(dir.path(), &config()).unwrap();
    assert_eq!(count(&db), 2);
}
