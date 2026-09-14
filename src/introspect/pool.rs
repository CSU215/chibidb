//! The buffer-pool panel's data: what is cached, what just happened to the
//! cache, and how close the WAL is to a checkpoint.
//!
//! Read-only, and deliberately cheap to poll: the counters are cumulative and
//! the frame table is a snapshot, so a console can sample twice a second and
//! work out rates by subtracting. Nothing here takes a pin, marks a page dirty
//! or touches the eviction order -- watching the pool must not change it.

use crate::result::json_string;
use crate::storage::{FrameView, PoolEvents, PoolStats};
use crate::Database;

/// An LSM table's shape, as `GET /api/metrics` reports it.
pub struct LsmTable {
    pub table: String,
    /// Live SSTable count per level, level 0 first.
    pub levels: Vec<usize>,
    pub memtable_bytes: u64,
}

/// Everything `GET /api/metrics` reports.
pub struct Metrics {
    pub database: String,
    pub pool: PoolStats,
    pub wal_bytes: u64,
    pub wal_threshold: u64,
    pub lsm: Vec<LsmTable>,
}

pub fn metrics(database: &str, db: &Database) -> Metrics {
    let mut lsm = Vec::new();
    let catalog = db.catalog();
    for meta in catalog.table_metas() {
        let Ok(table) = catalog.table(&meta.name) else {
            continue;
        };
        let Some(stats) = table.engine().lsm_stats() else {
            continue;
        };
        lsm.push(LsmTable {
            table: meta.name.clone(),
            levels: stats.levels,
            memtable_bytes: stats.memtable_bytes,
        });
    }
    Metrics {
        database: database.to_string(),
        pool: db.buffer_pool_stats(),
        wal_bytes: db.wal_bytes(),
        wal_threshold: db.wal_checkpoint_threshold(),
        lsm,
    }
}

pub fn frames(db: &Database) -> Vec<FrameView> {
    db.pool.frames()
}

pub fn events(db: &Database, since: u64) -> PoolEvents {
    db.pool.events(since)
}

impl Metrics {
    pub fn to_json(&self) -> String {
        let lsm: Vec<String> = self
            .lsm
            .iter()
            .map(|t| {
                let levels: Vec<String> = t.levels.iter().map(|n| n.to_string()).collect();
                format!(
                    "{{\"table\":{},\"levels\":[{}],\"memtable_bytes\":{}}}",
                    json_string(&t.table),
                    levels.join(","),
                    t.memtable_bytes
                )
            })
            .collect();
        // The pool is reported as cumulative counters, not as a hit rate: a
        // window is the reader's business, and computing it here would make the
        // series depend on how often the reader asks.
        format!(
            "{{\"database\":{},\"pool\":{{\"capacity\":{},\"resident\":{},\"hits\":{},\"misses\":{},\"evictions\":{},\"dirty_evictions\":{},\"hit_rate\":{}}},\"wal\":{{\"bytes\":{},\"threshold\":{}}},\"lsm\":[{}]}}",
            json_string(&self.database),
            self.pool.capacity,
            self.pool.resident,
            self.pool.hits,
            self.pool.misses,
            self.pool.evictions,
            self.pool.dirty_evictions,
            self.pool.hit_rate(),
            self.wal_bytes,
            self.wal_threshold,
            lsm.join(","),
        )
    }
}

/// The resident frames, newest key last. `accessed` is the CLOCK reference
/// bit: on a clock pool it is what the next eviction will look at.
pub fn frames_json(frames: &[FrameView]) -> String {
    let entries: Vec<String> = frames
        .iter()
        .map(|f| {
            format!(
                "{{\"file\":{},\"page\":{},\"pins\":{},\"dirty\":{},\"accessed\":{}}}",
                f.file, f.page, f.pins, f.dirty, f.accessed
            )
        })
        .collect();
    format!("{{\"frames\":[{}]}}", entries.join(","))
}

pub fn events_json(events: &PoolEvents) -> String {
    let entries: Vec<String> = events
        .events
        .iter()
        .map(|e| {
            format!(
                "{{\"seq\":{},\"kind\":{},\"file\":{},\"page\":{},\"pins\":{},\"dirty\":{}}}",
                e.seq,
                json_string(e.kind.name()),
                e.file,
                e.page,
                e.pins,
                e.dirty
            )
        })
        .collect();
    format!(
        "{{\"events\":[{}],\"next\":{},\"truncated\":{}}}",
        entries.join(","),
        events.next,
        events.truncated
    )
}
