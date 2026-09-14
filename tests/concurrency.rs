use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chibidb::config::Config;
use chibidb::instance::Instance;
use chibidb::value::Value;
use chibidb::{Database, ResultSet, Session};

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
fn instance_row_locks_block_the_second_writer() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.transaction.lock_timeout_ms = 100;
    let inst = Instance::open(dir.path(), &cfg).unwrap();
    inst.create_database("a").unwrap();
    let mut a = Session::new();
    let mut b = Session::new();

    inst.execute_with(&mut a, "use a;").unwrap();
    inst.execute_with(&mut a, "create table t (id int);").unwrap();
    inst.execute_with(&mut a, "insert into t values (1);").unwrap();

    inst.execute_with(&mut a, "begin;").unwrap();
    inst.execute_with(&mut a, "update t set id = 2 where id = 1;").unwrap();

    // BEGIN is not blocked (there is no whole-database lock); the write to the
    // same row waits on a's row lock and times out.
    inst.execute_with(&mut b, "use a;").unwrap();
    inst.execute_with(&mut b, "begin;").unwrap();
    let err = inst.execute_with(&mut b, "update t set id = 3 where id = 1;").unwrap_err();
    assert!(err.to_string().contains("lock wait timeout"), "{err}");

    // a can still commit while b waits (no lock-order deadlock), then b retries
    inst.execute_with(&mut a, "commit;").unwrap();
    inst.execute_with(&mut b, "update t set id = 3 where id = 1;").unwrap();
    inst.execute_with(&mut b, "commit;").unwrap();
}

#[test]
fn concurrent_autocommit_writes_stay_correct() {
    // DML now shares the database lock; distinct rows are serialized per row
    // by the lock manager, so concurrent writers must not lose or corrupt data.
    let dir = tempfile::tempdir().unwrap();
    let inst = Arc::new(Instance::open(dir.path(), &Config::default()).unwrap());
    inst.create_database("a").unwrap();
    inst.with_database_mut("a", |db| db.execute_sql("create table t (id int);").unwrap())
        .unwrap();

    std::thread::scope(|scope| {
        for worker in 0..4u32 {
            let inst = Arc::clone(&inst);
            scope.spawn(move || {
                let mut session = Session::new();
                inst.execute_with(&mut session, "use a;").unwrap();
                for i in 0..200u32 {
                    inst.execute_with(
                        &mut session,
                        &format!("insert into t values ({});", worker * 1000 + i),
                    )
                    .unwrap();
                }
            });
        }
    });

    assert_eq!(count(&inst, "a"), 800);
}

#[test]
fn concurrent_increments_do_not_lose_updates() {
    // Eight writers hammer one row with an atomic increment under the default
    // read committed. EPQ must re-read the latest version and re-apply, so the
    // final value equals the number of successful increments and nobody aborts.
    let db = Arc::new(Database::open_in_memory().unwrap());
    db.execute_sql("create table t (id int primary key, n int);").unwrap();
    db.execute_sql("insert into t values (1, 0);").unwrap();

    let ok = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let db = Arc::clone(&db);
            let ok = Arc::clone(&ok);
            scope.spawn(move || {
                let mut session = Session::new();
                for _ in 0..50 {
                    if db
                        .execute_sql_with(&mut session, "update t set n = n + 1 where id = 1;")
                        .is_ok()
                    {
                        ok.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });
        }
    });

    let successes = ok.load(Ordering::SeqCst) as i64;
    let n = match db.execute_sql("select n from t where id = 1;").unwrap().remove(0) {
        ResultSet::Rows { rows, .. } => match rows[0][0] {
            Value::Int(v) => v,
            ref other => panic!("expected int, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(n, successes, "every successful increment must be reflected");
    assert_eq!(successes, 400, "read committed must retry, not abort, under contention");
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
