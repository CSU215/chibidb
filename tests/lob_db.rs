use chibidb::config::Config;
use chibidb::value::Value;
use chibidb::{Database, ResultSet};

fn lob_config() -> Config {
    let mut cfg = Config::default();
    cfg.storage.inline_lob_limit = 64;
    cfg
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute_sql(sql).unwrap().remove(0) {
        ResultSet::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn long_strings_are_externalized_and_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with_config(dir.path(), &lob_config()).unwrap();
    db.execute_sql("create table t (id int, body text);").unwrap();

    let big = "x".repeat(200);
    db.execute_sql(&format!("insert into t values (1, '{big}');")).unwrap();
    assert_eq!(rows(&db, "select body from t where id = 1;"), [[Value::Str(big)]]);
    // the value went to a lob file rather than the row
    let lobs = std::fs::read_dir(dir.path().join("lobs")).unwrap().count();
    assert_eq!(lobs, 1);
}

#[test]
fn a_value_larger_than_a_page_is_supported() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with_config(dir.path(), &lob_config()).unwrap();
    db.execute_sql("create table t (id int, body text);").unwrap();

    // far beyond one 8KB page; only possible because it is stored out of line
    let huge = "y".repeat(20_000);
    db.execute_sql(&format!("insert into t values (1, '{huge}');")).unwrap();
    assert_eq!(rows(&db, "select body from t where id = 1;"), [[Value::Str(huge)]]);
}

#[test]
fn externalized_values_survive_a_checkpoint_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = lob_config();
    let big = "z".repeat(500);
    {
        let db = Database::open_with_config(dir.path(), &cfg).unwrap();
        db.execute_sql("create table t (id int, body text);").unwrap();
        db.execute_sql(&format!("insert into t values (1, '{big}');")).unwrap();
        db.flush().unwrap();
    }
    let db = Database::open_with_config(dir.path(), &cfg).unwrap();
    assert_eq!(rows(&db, "select body from t where id = 1;"), [[Value::Str(big)]]);
}

fn lob_count(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir.join("lobs")).map(|it| it.count()).unwrap_or(0)
}

#[test]
fn vacuum_reclaims_lob_files() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with_config(dir.path(), &lob_config()).unwrap();
    db.execute_sql("create table t (id int, body text);").unwrap();

    let big = "q".repeat(300);
    db.execute_sql(&format!("insert into t values (1, '{big}');")).unwrap();
    assert_eq!(lob_count(dir.path()), 1);

    db.execute_sql("delete from t;").unwrap();
    // delete-marked, still visible to snapshots: the object is kept
    assert_eq!(lob_count(dir.path()), 1);

    db.execute_sql("vacuum;").unwrap();
    assert_eq!(lob_count(dir.path()), 0);
}

#[test]
fn rollback_reclaims_a_lob_insert() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with_config(dir.path(), &lob_config()).unwrap();
    db.execute_sql("create table t (id int, body text);").unwrap();

    let mut session = chibidb::Session::new();
    let big = "r".repeat(300);
    db.execute_sql_with(&mut session, "begin;").unwrap();
    db.execute_sql_with(&mut session, &format!("insert into t values (1, '{big}');")).unwrap();
    assert_eq!(lob_count(dir.path()), 1);
    db.execute_sql_with(&mut session, "rollback;").unwrap();
    assert_eq!(lob_count(dir.path()), 0);
}

#[test]
fn update_then_vacuum_reclaims_the_old_lob() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with_config(dir.path(), &lob_config()).unwrap();
    db.execute_sql("create table t (id int, body text);").unwrap();

    let big = "u".repeat(300);
    db.execute_sql(&format!("insert into t values (1, '{big}');")).unwrap();
    assert_eq!(lob_count(dir.path()), 1);

    // the old version keeps its object until it is purged
    db.execute_sql("update t set body = 'tiny' where id = 1;").unwrap();
    assert_eq!(lob_count(dir.path()), 1);

    db.execute_sql("vacuum;").unwrap();
    assert_eq!(lob_count(dir.path()), 0);
    assert_eq!(rows(&db, "select body from t where id = 1;"), [[Value::Str("tiny".into())]]);
}

#[test]
fn externalized_values_recover_from_wal_after_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = lob_config();
    let big = "w".repeat(500);
    {
        let db = Database::open_with_config(dir.path(), &cfg).unwrap();
        db.execute_sql("create table t (id int, body text);").unwrap();
        db.execute_sql(&format!("insert into t values (1, '{big}');")).unwrap();
        db.simulate_crash();
    }
    let db = Database::open_with_config(dir.path(), &cfg).unwrap();
    assert_eq!(rows(&db, "select body from t where id = 1;"), [[Value::Str(big)]]);
}
