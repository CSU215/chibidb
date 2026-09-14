//! Transaction lifecycle and bookkeeping.
//!
//! Owns the commit counter and the two id sets the MVCC engine needs:
//! `committed` (transactions whose writes any new snapshot may see) and `open`
//! (transactions still in flight, which block checkpoints). Keeping them here
//! rather than on `Database` centralizes snapshot creation and the
//! first-committer-wins / 2PL checks that read them.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use parking_lot::RwLock;

/// Transaction id source plus the committed/open bookkeeping.
pub struct TransactionManager {
    next_id: AtomicU32,
    /// Monotonic commit counter. A checkpoint compares it across its flush to
    /// notice a transaction that committed while the flush was in flight,
    /// without having to read `committed` (which would invert the
    /// `commit` -> `open` lock order).
    commits: AtomicU64,
    committed: RwLock<HashSet<u32>>,
    open: RwLock<HashSet<u32>>,
}

impl TransactionManager {
    /// Resumes from a recovered state: the next free id and the committed set.
    pub fn new(next_id: u32, committed: HashSet<u32>) -> Self {
        Self {
            next_id: AtomicU32::new(next_id),
            commits: AtomicU64::new(0),
            committed: RwLock::new(committed),
            open: RwLock::new(HashSet::new()),
        }
    }

    /// Reserves the next transaction id.
    pub fn allocate(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Reserves an id and marks it open in one step. Splitting this into
    /// `allocate` + `insert_open` leaves a window where a transaction owns an
    /// id but is invisible to a checkpoint's no-open check.
    pub fn begin_open(&self) -> u32 {
        let mut open = self.open.write();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        open.insert(id);
        id
    }

    /// Never hands a recovered transaction's id to a new transaction.
    pub fn ensure_next_id_at_least(&self, id: u32) {
        if id >= self.next_id.load(Ordering::SeqCst) {
            self.next_id.store(id.saturating_add(1), Ordering::SeqCst);
        }
    }

    pub fn next_id(&self) -> u32 {
        self.next_id.load(Ordering::SeqCst)
    }

    /// A copy of the committed set: the snapshot a new transaction sees.
    pub fn snapshot(&self) -> HashSet<u32> {
        self.committed.read().clone()
    }

    pub fn committed_ids(&self) -> Vec<u32> {
        self.committed.read().iter().copied().collect()
    }

    pub fn contains_committed(&self, id: u32) -> bool {
        self.committed.read().contains(&id)
    }

    pub fn insert_open(&self, id: u32) {
        self.open.write().insert(id);
    }

    pub fn remove_open(&self, id: u32) {
        self.open.write().remove(&id);
    }

    /// Records a commit and clears the open marker.
    pub fn commit(&self, id: u32) {
        // Bump the counter first: a checkpoint holding the open set must see
        // this commit even though it cannot yet remove the open marker.
        self.commits.fetch_add(1, Ordering::SeqCst);
        self.committed.write().insert(id);
        self.open.write().remove(&id);
    }

    pub fn extend_committed(&self, ids: impl IntoIterator<Item = u32>) {
        self.committed.write().extend(ids);
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
    pub fn with_open_set<R>(&self, f: impl FnOnce(&HashSet<u32>) -> R) -> R {
        f(&self.open.write())
    }

    /// Whether any transaction other than `id` is open (blocks DDL/checkpoint).
    pub fn has_open_excluding(&self, id: u32) -> bool {
        self.open.read().iter().any(|&open| open != id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_moves_a_transaction_into_the_snapshot() {
        let tm = TransactionManager::new(1, HashSet::new());
        let a = tm.allocate();
        let b = tm.allocate();
        assert!(b > a);
        assert!(tm.snapshot().is_empty());

        tm.insert_open(a);
        assert!(!tm.no_open_transactions());
        assert!(tm.snapshot().is_empty(), "an open transaction is not yet visible");

        tm.commit(a);
        assert!(tm.contains_committed(a));
        assert!(tm.no_open_transactions());
        assert!(tm.snapshot().contains(&a));
    }

    #[test]
    fn begin_open_registers_the_id_before_any_work_starts() {
        let tm = TransactionManager::new(1, HashSet::new());
        let id = tm.begin_open();
        assert!(!tm.no_open_transactions(), "begin_open marks the id open");
        assert_eq!(tm.committed_count(), 0);
        tm.commit(id);
        assert!(tm.no_open_transactions());
        assert_eq!(tm.committed_count(), 1);
    }

    #[test]
    fn with_open_set_excludes_a_concurrent_begin() {
        let tm = TransactionManager::new(1, HashSet::new());
        tm.begin_open();
        let seen = tm.with_open_set(|open| open.len());
        assert_eq!(seen, 1);
        assert!(!tm.no_open_transactions());
    }

    #[test]
    fn ensure_next_id_never_reuses_a_recovered_id() {
        let tm = TransactionManager::new(1, HashSet::new());
        tm.ensure_next_id_at_least(10);
        assert!(tm.allocate() > 10);
    }

    #[test]
    fn has_open_excluding_ignores_the_caller() {
        let tm = TransactionManager::new(1, HashSet::new());
        tm.insert_open(1);
        assert!(!tm.has_open_excluding(1));
        tm.insert_open(2);
        assert!(tm.has_open_excluding(1));
    }
}
