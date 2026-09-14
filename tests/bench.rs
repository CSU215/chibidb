//! A tiny dependency-free micro-benchmark contrasting B+ tree point/range
//! scans with a full table scan. Ignored by default so it does not slow the
//! normal suite; run explicitly with:
//!
//!     cargo test --release --test bench -- --ignored --nocapture

use chaoticdb::Database;
use chaoticdb::config::{Config, ExecutionMode};
use std::time::{Duration, Instant};

const N: i64 = 50_000;

fn build() -> Database {
    build_mode(ExecutionMode::Volcano)
}

fn build_mode(mode: ExecutionMode) -> Database {
    build_mode_rows(mode, N)
}

fn build_mode_rows(mode: ExecutionMode, n: i64) -> Database {
    let mut config = Config::default();
    config.execution.mode = mode;
    let db = Database::open_in_memory_with_config(&config).unwrap();
    db.execute_sql("create table t (id int, tag int);").unwrap();
    let chunk = 500i64;
    let mut i = 0i64;
    while i < n {
        let mut sql = String::from("insert into t values ");
        for j in i..(i + chunk).min(n) {
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

/// Fastest single call, which tolerates CPU clock drift between runs.
fn per_op_min(iterations: u32, mut f: impl FnMut()) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..iterations {
        let start = Instant::now();
        f();
        best = best.min(start.elapsed());
    }
    best
}

/// `col + 0` defeats the sargable rule, forcing a full table scan.
fn bench(db: &Database, label: &str, indexed: &str, scanned: &str, indexed_iters: u32, scan_iters: u32) {
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
    let db = build();
    println!("rows: {N}");
    bench(
        &db,
        "point id = 12345",
        "select tag from t where id = 12345;",
        "select tag from t where id + 0 = 12345;",
        2000,
        50,
    );
    bench(
        &db,
        "narrow id < 100",
        "select id from t where id < 100;",
        "select id from t where id + 0 < 100;",
        2000,
        50,
    );
    bench(
        &db,
        "range 10000..10100",
        "select id from t where id >= 10000 and id < 10100;",
        "select id from t where id + 0 >= 10000 and id + 0 < 10100;",
        20,
        50,
    );
    bench(
        &db,
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
    let db = build();
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

/// Vectorized (chunk) vs row (volcano) execution on the same 50k-row table.
#[test]
#[ignore = "micro-benchmark; run with --ignored --nocapture"]
fn volcano_vs_chunk() {
    let volcano = build_mode(ExecutionMode::Volcano);
    let chunk = build_mode(ExecutionMode::Chunk);
    println!("rows: {N}");
    let cases: [(&str, &str, u32); 6] = [
        ("scan+project", "select id, tag from t;", 5),
        ("filter tag = 3", "select id from t where tag = 3;", 10),
        ("sum(tag)", "select sum(tag) from t;", 10),
        ("5 aggregates", "select count(*), sum(tag), avg(tag), min(tag), max(tag) from t;", 10),
        ("group by tag", "select tag, count(*) from t group by tag;", 5),
        ("order by tag", "select id from t order by tag;", 3),
    ];
    for (label, sql, iterations) in cases {
        let row = per_op(iterations, || {
            volcano.execute_sql(sql).unwrap();
        });
        let vec = per_op(iterations, || {
            chunk.execute_sql(sql).unwrap();
        });
        println!(
            "{label:<16} volcano {row:>12?}   chunk {vec:>12?}   {:>5.2}x",
            row.as_secs_f64() / vec.as_secs_f64()
        );
    }
}

/// Per-row cost of `sum(v)`, the workload the chunk aggregate path targets.
/// Seeded with fewer rows than the other benches because table building, not
/// the scan, dominates the runtime.
#[test]
#[ignore = "micro-benchmark; run with --ignored --nocapture"]
fn chunk_aggregate_throughput() {
    const ROWS: i64 = 10_000;
    let volcano = build_mode_rows(ExecutionMode::Volcano, ROWS);
    let chunk = build_mode_rows(ExecutionMode::Chunk, ROWS);
    println!("rows: {ROWS}");
    for (mode, db) in [("volcano", &volcano), ("chunk", &chunk)] {
        let elapsed = per_op_min(200, || {
            db.execute_sql("select sum(tag) from t;").unwrap();
        });
        let ns = elapsed.as_secs_f64() * 1e9 / ROWS as f64;
        println!("{mode:<8} sum(tag) {elapsed:>12?}   {ns:>6.2} ns/row");
    }
}

/// Hash join: streaming (chunk) vs materialized (volcano) on 50k x 50k rows.
#[test]
#[ignore = "micro-benchmark; run with --ignored --nocapture"]
fn volcano_vs_chunk_join() {
    fn setup(mode: ExecutionMode) -> Database {
        let mut config = Config::default();
        config.execution.mode = mode;
        let db = Database::open_in_memory_with_config(&config).unwrap();
        db.execute_sql("create table a (id int, v int);").unwrap();
        db.execute_sql("create table b (id int, w int);").unwrap();
        let chunk = 500i64;
        let mut i = 0i64;
        while i < N {
            let mut sa = String::from("insert into a values ");
            let mut sb = String::from("insert into b values ");
            for j in i..(i + chunk).min(N) {
                if j > i {
                    sa.push(',');
                    sb.push(',');
                }
                sa.push_str(&format!("({j},{})", j % 10));
                sb.push_str(&format!("({j},{})", j % 7));
            }
            sa.push(';');
            sb.push(';');
            db.execute_sql(&sa).unwrap();
            db.execute_sql(&sb).unwrap();
            i += chunk;
        }
        db
    }

    let volcano = setup(ExecutionMode::Volcano);
    let chunk = setup(ExecutionMode::Chunk);
    println!("rows: {N} x {N}");
    let sql = "select count(*) from a join b on a.id = b.id;";
    let row = per_op(3, || {
        volcano.execute_sql(sql).unwrap();
    });
    let vec = per_op(3, || {
        chunk.execute_sql(sql).unwrap();
    });
    println!(
        "join count(*)    volcano {row:>12?}   chunk {vec:>12?}   {:>5.2}x",
        row.as_secs_f64() / vec.as_secs_f64()
    );
}

/// Hash join throughput on two 50k-row tables.
#[test]
#[ignore = "micro-benchmark; run with --ignored --nocapture"]
fn hash_join_throughput() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table a (id int, v int);").unwrap();
    db.execute_sql("create table b (id int, w int);").unwrap();
    let chunk = 500i64;
    let mut i = 0i64;
    while i < N {
        let mut sa = String::from("insert into a values ");
        let mut sb = String::from("insert into b values ");
        for j in i..(i + chunk).min(N) {
            if j > i {
                sa.push(',');
                sb.push(',');
            }
            sa.push_str(&format!("({j},{})", j % 10));
            sb.push_str(&format!("({j},{})", j % 7));
        }
        sa.push(';');
        sb.push(';');
        db.execute_sql(&sa).unwrap();
        db.execute_sql(&sb).unwrap();
        i += chunk;
    }
    let elapsed = per_op(5, || {
        db.execute_sql("select count(*) from a join b on a.id = b.id;")
            .unwrap();
    });
    println!("hash join count(*)  50k x 50k   {elapsed:?}");
}
