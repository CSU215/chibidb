//! Serializable Snapshot Isolation conflict tracking (a simplified SSI).
//!
//! Reads are tracked at **table granularity** — a conservative predicate lock:
//! any concurrent write to a table a transaction read is treated as a possible
//! rw-antidependency. That catches phantoms (which a row-level scheme would
//! miss without range locks) at the cost of aborting some schedules that a
//! full SSI would allow.
//!
//! An rw-antidependency `R -> W` means `R` read a table that `W` wrote. Under
//! snapshot isolation an anomaly requires a cycle of such dependencies, so a
//! transaction that reaches commit while sitting on a cycle is aborted with a
//! serialization failure. Edges of committed transactions are retained until
//! no serializable transaction is open, so a cycle completed by a later
//! committer is still detected.
//!
//! Only serializable transactions participate; mixing isolation levels weakens
//! the guarantee to "serializable among serializable transactions", as in
//! PostgreSQL.

use std::collections::{HashMap, HashSet};

use parking_lot::Mutex;

#[derive(Default)]
pub struct Ssi {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Open serializable transactions.
    active: HashSet<u64>,
    /// table -> open serializable transactions that read it.
    readers: HashMap<String, HashSet<u64>>,
    /// table -> open serializable transactions that wrote it.
    writers: HashMap<String, HashSet<u64>>,
    /// reader -> writers it has an rw-antidependency with.
    out: HashMap<u64, HashSet<u64>>,
}

impl Ssi {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a serializable transaction as open.
    pub fn begin(&self, id: u64) {
        self.inner.lock().active.insert(id);
    }

    /// Records that `id` read `table`. No-op unless `id` is an open
    /// serializable transaction.
    pub fn note_read(&self, id: u64, table: &str) {
        let mut inner = self.inner.lock();
        if !inner.active.contains(&id) {
            return;
        }
        inner.readers.entry(table.to_string()).or_default().insert(id);
        if let Some(writers) = inner.writers.get(table) {
            let mut edges = Vec::new();
            for &w in writers {
                if w != id {
                    edges.push(w);
                }
            }
            if !edges.is_empty() {
                let out = inner.out.entry(id).or_default();
                out.extend(edges);
            }
        }
    }

    /// Records that `id` wrote `table`, adding `reader -> id` edges for every
    /// other open serializable transaction that read it.
    pub fn note_write(&self, id: u64, table: &str) {
        let mut inner = self.inner.lock();
        if !inner.active.contains(&id) {
            return;
        }
        inner.writers.entry(table.to_string()).or_default().insert(id);
        if let Some(readers) = inner.readers.get(table) {
            let mut edges = Vec::new();
            for &r in readers {
                if r != id {
                    edges.push(r);
                }
            }
            for r in edges {
                inner.out.entry(r).or_default().insert(id);
            }
        }
    }

    /// Drops a committed transaction. Its edges are kept until no serializable
    /// transaction is open, so a later committer can still detect a cycle that
    /// runs through it; once none is open no cycle can be completed and the
    /// graph is discarded wholesale.
    pub fn commit(&self, id: u64) {
        let mut inner = self.inner.lock();
        self.forget(&mut inner, id);
        if inner.active.is_empty() {
            inner.out.clear();
        }
    }

    /// Drops an aborted transaction and every edge that touches it: its work
    /// was undone, so it takes part in no dependency.
    pub fn abort(&self, id: u64) {
        let mut inner = self.inner.lock();
        self.forget(&mut inner, id);
        inner.out.remove(&id);
        for set in inner.out.values_mut() {
            set.remove(&id);
        }
        if inner.active.is_empty() {
            inner.out.clear();
        }
    }

    fn forget(&self, inner: &mut Inner, id: u64) {
        inner.active.remove(&id);
        for set in inner.readers.values_mut() {
            set.remove(&id);
        }
        inner.readers.retain(|_, s| !s.is_empty());
        for set in inner.writers.values_mut() {
            set.remove(&id);
        }
        inner.writers.retain(|_, s| !s.is_empty());
    }

    /// Whether `id` lies on a cycle of rw-antidependencies.
    pub fn has_cycle_through(&self, id: u64) -> bool {
        let inner = self.inner.lock();
        let mut stack = vec![id];
        let mut seen = HashSet::new();
        while let Some(cur) = stack.pop() {
            let Some(nexts) = inner.out.get(&cur) else { continue };
            for &next in nexts {
                if next == id {
                    return true;
                }
                if seen.insert(next) {
                    stack.push(next);
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_write_pair_is_not_a_cycle() {
        let ssi = Ssi::new();
        ssi.begin(1);
        ssi.begin(2);
        ssi.note_read(1, "t");
        ssi.note_write(2, "t");
        // 1 read, 2 wrote: the order is forced (1 before 2), no anomaly
        assert!(!ssi.has_cycle_through(1));
        assert!(!ssi.has_cycle_through(2));
    }

    #[test]
    fn write_skew_forms_a_cycle() {
        let ssi = Ssi::new();
        ssi.begin(1);
        ssi.begin(2);
        // both read the table, then both write it
        ssi.note_read(1, "t");
        ssi.note_read(2, "t");
        ssi.note_write(1, "t");
        ssi.note_write(2, "t");
        assert!(ssi.has_cycle_through(1));
        assert!(ssi.has_cycle_through(2));
    }

    #[test]
    fn read_only_and_writer_both_commit() {
        let ssi = Ssi::new();
        ssi.begin(1);
        ssi.begin(2);
        ssi.note_read(1, "t");
        ssi.note_write(2, "t");
        assert!(!ssi.has_cycle_through(2));
        ssi.commit(1);
        ssi.commit(2);
    }

    #[test]
    fn aborting_one_side_of_a_cycle_clears_its_edges() {
        let ssi = Ssi::new();
        ssi.begin(1);
        ssi.begin(2);
        ssi.note_read(1, "t");
        ssi.note_read(2, "t");
        ssi.note_write(1, "t");
        ssi.note_write(2, "t");
        // the first committer is rejected and aborts
        assert!(ssi.has_cycle_through(1));
        ssi.abort(1);
        // the survivor no longer sees a cycle
        assert!(!ssi.has_cycle_through(2));
    }

    #[test]
    fn the_graph_is_dropped_when_no_transaction_remains() {
        let ssi = Ssi::new();
        ssi.begin(1);
        ssi.begin(2);
        ssi.note_read(1, "t");
        ssi.note_write(2, "t");
        ssi.commit(1);
        ssi.commit(2);
        // a fresh pair starts from an empty graph
        ssi.begin(3);
        ssi.note_read(3, "t");
        assert!(!ssi.has_cycle_through(3));
    }
}
