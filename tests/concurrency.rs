use std::sync::Arc;

use chibidb::config::Config;
use chibidb::instance::Instance;
use chibidb::value::Value;
use chibidb::{ResultSet, Session};

fn count(inst: &Instance, db_name: &str) -> i64 {
    inst.with_database_mut(db_name, |db| {
        let rs = db.execute_sql("select count(*) from t;").unwrap();
        match &rs[0] {
            ResultSet::Rows { rows, .. } => match rows[0][0] {
                Value::Int(n) => n,
                ref v => panic!("expected int, got {v:?}"),
            },
            other => panic!("expected rows, got {other:?}"),
        }
    })
    .unwrap()
}

#[test]
fn databases_are_used_concurrently_and_stay_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let inst = Arc::new(Instance::open(dir.path(), &Config::default()).unwrap());
    inst.create_database("a").unwrap();
    inst.create_database("b").unwrap();

    std::thread::scope(|scope| {
        for name in ["a", "b"] {
            let inst = Arc::clone(&inst);
            scope.spawn(move || {
                inst.with_database_mut(name, |db| {
                    db.execute_sql("create table t (id int);").unwrap();
                    for i in 0..500 {
                        db.execute_sql(&format!("insert into t values ({i});")).unwrap();
                    }
                })
                .unwrap();
            });
        }
    });

    assert_eq!(count(&inst, "a"), 500);
    assert_eq!(count(&inst, "b"), 500);
}

#[test]
fn same_database_writes_serialize_safely() {
    let dir = tempfile::tempdir().unwrap();
    let inst = Arc::new(Instance::open(dir.path(), &Config::default()).unwrap());
    inst.create_database("a").unwrap();
    inst.with_database_mut("a", |db| db.execute_sql("create table t (id int);").unwrap())
        .unwrap();

    std::thread::scope(|scope| {
        for worker in 0..4 {
            let inst = Arc::clone(&inst);
            scope.spawn(move || {
                for i in 0..200 {
                    inst.with_database_mut("a", |db| {
                        db.execute_sql(&format!("insert into t values ({});", worker * 1000 + i))
                            .unwrap();
                    })
                    .unwrap();
                }
            });
        }
    });

    assert_eq!(count(&inst, "a"), 800);
}

#[test]
fn same_database_reads_run_concurrently() {
    let dir = tempfile::tempdir().unwrap();
    let inst = Arc::new(Instance::open(dir.path(), &Config::default()).unwrap());
    inst.create_database("a").unwrap();
    inst.with_database_mut("a", |db| {
        db.execute_sql("create table t (id int);").unwrap();
        for i in 0..200 {
            db.execute_sql(&format!("insert into t values ({i});")).unwrap();
        }
    })
    .unwrap();

    std::thread::scope(|scope| {
        for _ in 0..8 {
            let inst = Arc::clone(&inst);
            scope.spawn(move || {
                let mut session = Session::new();
                inst.execute_with(&mut session, "use a;").unwrap();
                for _ in 0..100 {
                    let rs =
                        inst.execute_with(&mut session, "select count(*) from t;").unwrap();
                    match &rs[0] {
                        ResultSet::Rows { rows, .. } => {
                            assert_eq!(rows[0][0], Value::Int(200))
                        }
                        other => panic!("expected rows, got {other:?}"),
                    }
                }
            });
        }
    });
}
