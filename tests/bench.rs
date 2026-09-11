//! A tiny dependency-free micro-benchmark contrasting B+ tree point/range
//! scans with a full table scan. Ignored by default so it does not slow the
//! normal suite; run explicitly with:
//!
//!     cargo test --release --test bench -- --ignored --nocapture

use chibidb::Database;
use std::time::{Duration, Instant};

const N: i64 = 50_000;

fn build() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int, tag int);").unwrap();
    let chunk = 500i64;
    let mut i = 0i64;
    while i < N {
        let mut sql = String::from("insert into t values ");
        for j in i..(i + chunk).min(N) {
            if j > i {
                sql.push(',');
            }
            sql.push_str(&format!("({j},{})", j % 10));
        }
        sql.push(';');
        db.execute_sql(&sql).unwrap();
        i += chunk;
    }
    db.execute_sql("create index idx_id on t (id);").unwrap();
    db
}

fn per_op(iterations: u32, mut f: impl FnMut()) -> Duration {
    let start = Instant::now();
    for _ in 0..iterations {
        f();
    }
    start.elapsed() / iterations
}

/// `col + 0` defeats the sargable rule, forcing a full table scan.
fn bench(db: &mut Database, label: &str, indexed: &str, scanned: &str, indexed_iters: u32, scan_iters: u32) {
    let idx = per_op(indexed_iters, || {
        db.execute_sql(indexed).unwrap();
    });
    let full = per_op(scan_iters, || {
        db.execute_sql(scanned).unwrap();
    });
    println!("{label:<22} index {idx:>12?}   full {full:>12?}");
}

#[test]
#[ignore = "micro-benchmark; run with --ignored --nocapture"]
fn index_vs_full_scan() {
    let mut db = build();
    println!("rows: {N}");
    bench(
        &mut db,
        "point id = 12345",
        "select tag from t where id = 12345;",
        "select tag from t where id + 0 = 12345;",
        2000,
        50,
    );
    bench(
        &mut db,
        "narrow id < 100",
        "select id from t where id < 100;",
        "select id from t where id + 0 < 100;",
        2000,
        50,
    );
    bench(
        &mut db,
        "range 10000..10100",
        "select id from t where id >= 10000 and id < 10100;",
        "select id from t where id + 0 >= 10000 and id + 0 < 10100;",
        20,
        50,
    );
    bench(
        &mut db,
        "ordered id > 49900",
        "select id from t where id > 49900 order by id;",
        "select id from t where id + 0 > 49900 order by id;",
        200,
        20,
    );
}

/// Baseline throughput of the current materialized executor. The volcano/chunk
/// operators added in P5 must be measured against these numbers.
#[test]
#[ignore = "micro-benchmark; run with --ignored --nocapture"]
fn materialized_execution_baseline() {
    let mut db = build();
    println!("rows: {N}");
    let cases: [(&str, &str, u32); 4] = [
        ("scan+project", "select id, tag from t;", 3),
        ("filter tag = 3", "select id from t where tag = 3;", 10),
        ("count(*)", "select count(*) from t;", 5),
        ("group by tag", "select tag, count(*) from t group by tag;", 5),
    ];
    for (label, sql, iterations) in cases {
        let elapsed = per_op(iterations, || {
            db.execute_sql(sql).unwrap();
        });
        println!("{label:<18} {elapsed:>12?}");
    }
}
