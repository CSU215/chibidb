//! Tiny dependency-free micro-benchmarks: B+ tree point/range scans vs a full
//! table scan, vectorized vs row execution, and the Heap vs LSM engines.
//! Ignored by default so they do not slow the normal suite; run explicitly:
//!
//!     cargo test --release --test bench -- --ignored --nocapture

use chaoticdb::config::{Config, ExecutionMode};
use chaoticdb::value::Value;
use chaoticdb::{Database, ResultSet};
use std::path::Path;
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

// ---------------------------------------------------------------------------
// Heap vs LSM engine, deliberately without any index so the comparison is the
// storage engine itself (page writes vs memtable/SSTable), not the B+ tree.
// ---------------------------------------------------------------------------

const ENGINE_ROWS: i64 = 100_000;
const ENGINE_BATCH: i64 = 500;

fn open_engine(engine: &str) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    db.execute_sql(&format!(
        "create table t (id int, grp int, name char(16), score int) engine = {engine};"
    ))
    .unwrap();
    (dir, db)
}

/// Bulk-loads `ENGINE_ROWS` rows in multi-value INSERT batches. No index, so
/// every row goes straight at the table storage.
fn insert_all(db: &Database) -> Duration {
    let start = Instant::now();
    let mut i = 0i64;
    while i < ENGINE_ROWS {
        let mut sql = String::from("insert into t values ");
        for j in i..(i + ENGINE_BATCH).min(ENGINE_ROWS) {
            if j > i {
                sql.push(',');
            }
            sql.push_str(&format!("({j},{},'n{}',{})", j % 10, j % 20, (j * 7) % 1000));
        }
        sql.push(';');
        db.execute_sql(&sql).unwrap();
        i += ENGINE_BATCH;
    }
    start.elapsed()
}

/// Runs a query expected to return a single integer in its first cell.
fn scalar_int(db: &Database, sql: &str) -> i64 {
    match db.execute_sql(sql).unwrap().remove(0) {
        ResultSet::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            ref other => panic!("expected int, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Recursive on-disk size of the data root, i.e. bytes the engine actually
/// persisted (tables + WAL + catalog), after a flush.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        // `std::fs::metadata` (a fresh stat) rather than `DirEntry::metadata`:
        // on Windows the directory-scan cache can report a still-open heap
        // file as 0 bytes.
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            total += dir_size(&p);
        } else {
            total += meta.len();
        }
    }
    total
}

struct EngineStats {
    insert: Duration,
    flush: Duration,
    footprint: u64,
    count: Duration,
    aggregate: Duration,
    point: Duration,
    filter: Duration,
    update_all: Duration,
    delete_pred: Duration,
    reopen: Duration,
    final_count: i64,
}

fn run_engine_bench(engine: &str) -> EngineStats {
    let (dir, db) = open_engine(engine);

    let insert = insert_all(&db);
    assert_eq!(
        scalar_int(&db, "select count(*) from t;"),
        ENGINE_ROWS,
        "{engine}: insert count"
    );

    let t = Instant::now();
    db.flush().unwrap();
    let flush = t.elapsed();
    let footprint = dir_size(dir.path());

    let count = per_op(5, || {
        db.execute_sql("select count(*) from t;").unwrap();
    });
    let aggregate = per_op(5, || {
        db.execute_sql("select sum(score), avg(score), min(score), max(score) from t;")
            .unwrap();
    });
    let point = per_op(10, || {
        db.execute_sql(&format!("select id, score from t where id = {};", ENGINE_ROWS / 2))
            .unwrap();
    });
    let filter = per_op(5, || {
        db.execute_sql("select id from t where grp = 3;").unwrap();
    });

    let t = Instant::now();
    db.execute_sql("update t set score = score + 1;").unwrap();
    let update_all = t.elapsed();
    assert_eq!(
        scalar_int(&db, "select count(*) from t;"),
        ENGINE_ROWS,
        "{engine}: count after update"
    );

    let t = Instant::now();
    db.execute_sql("delete from t where grp = 0;").unwrap();
    let delete_pred = t.elapsed();
    let final_count = scalar_int(&db, "select count(*) from t;");

    db.flush().unwrap();
    let t = Instant::now();
    drop(db);
    let reopened = Database::open(dir.path()).unwrap();
    let reopen = t.elapsed();
    assert_eq!(
        scalar_int(&reopened, "select count(*) from t;"),
        final_count,
        "{engine}: count survives reopen"
    );

    EngineStats {
        insert,
        flush,
        footprint,
        count,
        aggregate,
        point,
        filter,
        update_all,
        delete_pred,
        reopen,
        final_count,
    }
}

fn ms(d: Duration) -> String {
    format!("{:>9.1}ms", d.as_secs_f64() * 1e3)
}

#[test]
#[ignore = "micro-benchmark; run with --ignored --nocapture"]
fn lsm_vs_heap_no_index() {
    println!("rows: {ENGINE_ROWS}, no index");
    let heap = run_engine_bench("heap");
    let lsm = run_engine_bench("lsm");
    assert_eq!(
        heap.final_count, lsm.final_count,
        "engines disagree on row count"
    );

    let rows_per_s = |d: Duration| ENGINE_ROWS as f64 / d.as_secs_f64();
    let bytes_per_row = |b: u64| b as f64 / ENGINE_ROWS as f64;

    println!();
    println!("{:<28}{:>22}{:>22}", "phase", "heap", "lsm");
    println!(
        "{:<28}{:>22}{:>22}",
        "bulk insert (rows/s)",
        format!("{:.0}", rows_per_s(heap.insert)),
        format!("{:.0}", rows_per_s(lsm.insert)),
    );
    println!(
        "{:<28}{:>22}{:>22}",
        "insert wall time",
        ms(heap.insert),
        ms(lsm.insert)
    );
    println!(
        "{:<28}{:>22}{:>22}",
        "flush()",
        ms(heap.flush),
        ms(lsm.flush)
    );
    println!(
        "{:<28}{:>22}{:>22}",
        "on-disk total (bytes)",
        format!("{}", heap.footprint),
        format!("{}", lsm.footprint),
    );
    println!(
        "{:<28}{:>22}{:>22}",
        "on-disk bytes/row",
        format!("{:.1}", bytes_per_row(heap.footprint)),
        format!("{:.1}", bytes_per_row(lsm.footprint)),
    );
    println!(
        "{:<28}{:>22}{:>22}",
        "count(*)",
        ms(heap.count),
        ms(lsm.count)
    );
    println!(
        "{:<28}{:>22}{:>22}",
        "sum/avg/min/max",
        ms(heap.aggregate),
        ms(lsm.aggregate)
    );
    println!(
        "{:<28}{:>22}{:>22}",
        "point lookup (full scan)",
        ms(heap.point),
        ms(lsm.point)
    );
    println!(
        "{:<28}{:>22}{:>22}",
        "filter grp = 3",
        ms(heap.filter),
        ms(lsm.filter)
    );
    println!(
        "{:<28}{:>22}{:>22}",
        "update all rows",
        ms(heap.update_all),
        ms(lsm.update_all)
    );
    println!(
        "{:<28}{:>22}{:>22}",
        "delete grp = 0",
        ms(heap.delete_pred),
        ms(lsm.delete_pred)
    );
    println!(
        "{:<28}{:>22}{:>22}",
        "reopen + first count",
        ms(heap.reopen),
        ms(lsm.reopen)
    );
}
