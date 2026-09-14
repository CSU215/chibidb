//! Reproducible micro-benchmarks. These are `#[ignore]`d so they stay out of
//! the default regression run; invoke them explicitly:
//!
//! ```text
//! cargo test --test perf_stats -- --ignored --nocapture
//! ```

use chaoticdb::config::{Config, EvictionPolicy};
use chaoticdb::storage::lsm::PersistentLsm;
use chaoticdb::Database;

/// A leveled store must keep the number of live tables logarithmic in the
/// number of flushes (write amplification O(N log N)), not linear.
#[test]
#[ignore = "perf probe: run with --ignored --nocapture"]
fn lsm_live_table_count_grows_logarithmically() {
    let dir = tempfile::tempdir().unwrap();
    let mut lsm = PersistentLsm::open_with_trigger(dir.path(), 4096, 4).unwrap();

    let flushes = 64u32;
    let mut counts = Vec::new();
    for round in 0..flushes {
        for i in 0..200u32 {
            lsm.put(format!("k{round:03}{i:04}").into_bytes(), vec![b'x'; 64]);
        }
        lsm.flush().unwrap();
        counts.push(lsm.num_sstables());
    }

    println!("flushes -> live tables: {counts:?}");
    println!("final: {} tables after {flushes} flushes", lsm.num_sstables());

    // trigger-1 tables per level => at most ~log2(flushes) tables per level
    let bound = 4 * (((flushes as f64).log2() as usize) + 1);
    assert!(
        counts.iter().all(|&c| c <= bound),
        "live table counts {counts:?} exceeded the logarithmic bound {bound}"
    );
}

/// Under memory pressure the pool must serve some lookups from cache and
/// evict for others; this prints the observed hit rate.
#[test]
#[ignore = "perf probe: run with --ignored --nocapture"]
fn buffer_pool_hit_rate_under_pressure() {
    let (hits, misses, evictions) = hit_rate_probe(EvictionPolicy::Lru);
    assert!(hits > 0, "expected some cache hits");
    assert!(misses > 0, "expected some cache misses under pressure");
    assert!(evictions > 0, "a 16-frame pool should have evicted frames");
}

/// Runs the same point-lookup workload through a small pool and returns
/// `(hits, misses, evictions)` for the probe loop.
fn hit_rate_probe(policy: EvictionPolicy) -> (u64, u64, u64) {
    let mut cfg = Config::default();
    cfg.storage.buffer_pool_frames = 16;
    cfg.storage.eviction = policy;
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with_config(dir.path(), &cfg).unwrap();
    db.execute_sql("create table t (id int primary key, v char(64));").unwrap();

    let rows = 4000u32;
    for chunk in 0..(rows / 100) {
        let values: Vec<String> =
            (0..100).map(|i| format!("({}, 'v{}')", chunk * 100 + i, i)).collect();
        db.execute_sql(&format!("insert into t values {};", values.join(", "))).unwrap();
    }

    let before = db.buffer_pool_stats();
    let probes = 1500u32;
    for i in 0..probes {
        let id = (i * 7) % rows;
        db.execute_sql(&format!("select v from t where id = {id};")).unwrap();
    }
    let after = db.buffer_pool_stats();

    let hits = after.hits - before.hits;
    let misses = after.misses - before.misses;
    let evictions = after.evictions - before.evictions;
    let total = hits + misses;
    println!("{probes} point lookups over a 16-frame pool under {policy:?}:");
    println!("  hits={hits} misses={misses} evictions={evictions}");
    println!("  hit rate = {:.1}%", 100.0 * hits as f64 / total.max(1) as f64);
    (hits, misses, evictions)
}

/// Compares the three eviction policies on one workload. Prints only — no
/// threshold, because the point-lookup mix is not chosen to favour any one
/// policy and the numbers are machine-dependent.
#[test]
#[ignore = "perf probe: run with --ignored --nocapture"]
fn eviction_policy_hit_rate_comparison() {
    let mut rates = Vec::new();
    for policy in
        [EvictionPolicy::Lru, EvictionPolicy::Clock, EvictionPolicy::Fifo]
    {
        let (hits, misses, _) = hit_rate_probe(policy);
        rates.push((policy, 100.0 * hits as f64 / (hits + misses).max(1) as f64));
    }
    println!("\nsummary:");
    for (policy, rate) in rates {
        println!("  {policy:?}: {rate:.1}%");
    }
}
