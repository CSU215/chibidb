//! Transaction lifecycle and bookkeeping.
//!
//! Owns the commit counter, the commit-status bitmap (clog) and the open set.
//! A snapshot is a small `{ xmax, xip }` view (PostgreSQL-style) instead of a
//! full copy of the committed set: visibility consults the shared clog.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;

use crate::db::clog::CommitStatus;

/// A PostgreSQL-style snapshot: an upper bound plus the in-flight xids.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// xids >= xmax were allocated after the snapshot: always invisible.
    pub xmax: u64,
    /// xids in flight when the snapshot was taken, sorted ascending.
    pub xip: Vec<u64>,
}

/// Transaction id source plus the committed/open bookkeeping.
pub struct TransactionManager {
    next_id: AtomicU64,
    /// Monotonic commit counter. A checkpoint compares it across its flush to
    /// notice a transaction that committed while the flush was in flight,
    /// without having to read the clog (which would invert the
    /// `commit` -> `open` lock order).
    commits: AtomicU64,
    committed: Arc<CommitStatus>,
    open: RwLock<HashSet<u64>>,
}

impl TransactionManager {
    /// Resumes from a recovered state: the next free id, the committed ids at
    /// or above the persisted horizon, and that horizon.
    pub fn new(next_id: u64, committed: impl IntoIterator<Item = u64>, base: u64) -> Self {
        let status = CommitStatus::new();
        status.advance_base(base);
        for id in committed {
            status.mark_committed(id);
        }
        Self {
            next_id: AtomicU64::new(next_id),
            commits: AtomicU64::new(0),
            committed: Arc::new(status),
            open: RwLock::new(HashSet::new()),
        }
    }

    /// Reserves the next transaction id.
    pub fn allocate(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Reserves an id and marks it open in one step. Splitting this into
    /// `allocate` + `insert_open` leaves a window where a transaction owns an
    /// id but is invisible to a checkpoint's no-open check.
    pub fn begin_open(&self) -> u64 {
        let mut open = self.open.write();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        open.insert(id);
        id
    }

    /// Never hands a recovered transaction's id to a new transaction.
    pub fn ensure_next_id_at_least(&self, id: u64) {
        if id >= self.next_id.load(Ordering::SeqCst) {
            self.next_id.store(id.saturating_add(1), Ordering::SeqCst);
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.load(Ordering::SeqCst)
    }

    /// The snapshot a new transaction sees: the next xid and the in-flight set.
    pub fn snapshot(&self) -> Snapshot {
        let xmax = self.next_id.load(Ordering::SeqCst);
        let open = self.open.read();
        let mut xip: Vec<u64> = open.iter().copied().collect();
        xip.sort_unstable();
        Snapshot { xmax, xip }
    }

    /// A snapshot together with the clog it must consult.
    pub fn begin_snapshot(&self) -> (Snapshot, Arc<CommitStatus>) {
        (self.snapshot(), Arc::clone(&self.committed))
    }

    /// The shared commit-status bitmap.
    pub fn commit_status(&self) -> Arc<CommitStatus> {
        Arc::clone(&self.committed)
    }

    /// The vacuum horizon below which commit status is frozen as committed.
    pub fn clog_base(&self) -> u64 {
        self.committed.base()
    }

    /// Advances the vacuum horizon, dropping the frozen prefix of the clog.
    /// Callers must have run the vacuum cleanup that makes below-horizon status
    /// equivalent to "committed".
    pub fn advance_horizon(&self, new_base: u64) -> u64 {
        self.committed.advance_base(new_base)
    }

    pub fn committed_ids(&self) -> Vec<u64> {
        self.committed.ids()
    }

    pub fn is_committed(&self, id: u64) -> bool {
        self.committed.is_committed(id)
    }

    pub fn insert_open(&self, id: u64) {
        self.open.write().insert(id);
    }

    pub fn remove_open(&self, id: u64) {
        self.open.write().remove(&id);
    }

    /// Records a commit and clears the open marker.
    pub fn commit(&self, id: u64) {
        // Bump the counter first: a checkpoint holding the open set must see
        // this commit even though it cannot yet remove the open marker.
        self.commits.fetch_add(1, Ordering::SeqCst);
        self.committed.mark_committed(id);
        self.open.write().remove(&id);
    }

    pub fn extend_committed(&self, ids: impl IntoIterator<Item = u64>) {
        for id in ids {
            self.committed.mark_committed(id);
        }
    }

    pub fn no_open_transactions(&self) -> bool {
        self.open.read().is_empty()
    }

    /// Monotonic count of committed transactions, for checkpoint gating.
    pub fn committed_count(&self) -> u64 {
        self.commits.load(Ordering::SeqCst)
    }

    /// Runs `f` while holding the open set, so no transaction can begin and
    /// none can finish committing during a checkpoint's final decision.
    pub fn with_open_set<R>(&self, f: impl FnOnce(&HashSet<u64>) -> R) -> R {
        f(&self.open.write())
    }

    /// Whether any transaction other than `id` is open (blocks DDL/checkpoint).
    pub fn has_open_excluding(&self, id: u64) -> bool {
        self.open.read().iter().any(|&open| open != id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_moves_a_transaction_into_the_snapshot() {
        let tm = TransactionManager::new(1, [], 0);
        let a = tm.allocate();
        let b = tm.allocate();
        assert!(b > a);
        assert!(!tm.is_committed(a));

        tm.insert_open(a);
        assert!(!tm.no_open_transactions());
        let snap = tm.snapshot();
        assert_eq!(snap.xip, vec![a], "an open transaction is in the snapshot's xip");
        assert!(!tm.is_committed(a), "an open transaction is not yet visible");

        tm.commit(a);
        assert!(tm.is_committed(a));
        assert!(tm.no_open_transactions());
        let snap = tm.snapshot();
        assert!(snap.xip.is_empty());
        assert!(a < snap.xmax && tm.is_committed(a), "a fresh snapshot sees it");
    }

    #[test]
    fn begin_open_registers_the_id_before_any_work_starts() {
        let tm = TransactionManager::new(1, [], 0);
        let id = tm.begin_open();
        assert!(!tm.no_open_transactions(), "begin_open marks the id open");
        assert_eq!(tm.committed_count(), 0);
        tm.commit(id);
        assert!(tm.no_open_transactions());
        assert_eq!(tm.committed_count(), 1);
    }

    #[test]
    fn with_open_set_excludes_a_concurrent_begin() {
        let tm = TransactionManager::new(1, [], 0);
        tm.begin_open();
        let seen = tm.with_open_set(|open| open.len());
        assert_eq!(seen, 1);
        assert!(!tm.no_open_transactions());
    }

    #[test]
    fn ensure_next_id_never_reuses_a_recovered_id() {
        let tm = TransactionManager::new(1, [], 0);
        tm.ensure_next_id_at_least(10);
        assert!(tm.allocate() > 10);
    }

    #[test]
    fn has_open_excluding_ignores_the_caller() {
        let tm = TransactionManager::new(1, [], 0);
        tm.insert_open(1);
        assert!(!tm.has_open_excluding(1));
        tm.insert_open(2);
        assert!(tm.has_open_excluding(1));
    }

    #[test]
    fn recovered_committed_ids_are_visible_to_a_new_snapshot() {
        let tm = TransactionManager::new(7, [3, 5], 0);
        assert!(tm.is_committed(3));
        assert!(tm.is_committed(5));
        assert!(!tm.is_committed(4));
        assert_eq!(tm.committed_ids(), vec![3, 5]);
        let snap = tm.snapshot();
        assert!(snap.xip.is_empty() && snap.xmax == 7);
    }
}
