use chaoticdb::config::{Config, EngineKind};
use chaoticdb::value::Value;
use chaoticdb::{Database, ResultSet};

fn lsm_config() -> Config {
    let mut cfg = Config::default();
    cfg.storage.default_engine = EngineKind::Lsm;
    cfg
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute_sql(sql).unwrap().remove(0) {
        ResultSet::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn lsm_table_supports_dml_and_survives_checkpoint_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = lsm_config();
    {
        let db = Database::open_with_config(dir.path(), &cfg).unwrap();
        db.execute_sql("create table t (id int primary key, name char(8));").unwrap();
        db.execute_sql("insert into t values (1, 'a'), (2, 'b');").unwrap();
        db.execute_sql("update t set name = 'B' where id = 2;").unwrap();
        db.execute_sql("delete from t where id = 1;").unwrap();
        // flushes the LSM memtable to an SSTable and truncates the WAL
        db.flush().unwrap();
    }

    let db = Database::open_with_config(dir.path(), &cfg).unwrap();
    assert_eq!(
        rows(&db, "select id, name from t order by id;"),
        [[Value::Int(2), Value::Str("B".into())]]
    );
}

#[test]
fn lsm_table_recovers_from_wal_after_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = lsm_config();
    {
        let db = Database::open_with_config(dir.path(), &cfg).unwrap();
        db.execute_sql("create table t (id int primary key, name char(8));").unwrap();
        db.execute_sql("insert into t values (1, 'a'), (2, 'b');").unwrap();
        db.simulate_crash();
    }

    let db = Database::open_with_config(dir.path(), &cfg).unwrap();
    assert_eq!(rows(&db, "select count(*) from t;"), [[Value::Int(2)]]);
    assert_eq!(rows(&db, "select name from t where id = 2;"), [[Value::Str("b".into())]]);
}
