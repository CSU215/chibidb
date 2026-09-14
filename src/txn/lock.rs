//! Row-level lock manager: PostgreSQL-style two-phase locking for writes.
//!
//! Locks are keyed by `(table, rid)`, held by a session until its transaction
//! ends, and released by hand-off to the front waiter (FIFO). A blocking
//! acquire gives up after `lock_timeout_ms`; once a waiter has been stuck for
//! `deadlock_timeout_ms` it checks the wait-for graph and aborts itself if its
//! chain comes back to it.
//!
//! Only Step 3 wires this in behind the existing per-database write lock; real
//! contention appears once that coarse lock is removed.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::storage::Rid;
use crate::{Error, Result};

type Key = (String, Rid);

struct LockEntry {
    holder: Option<u64>,
    waiters: VecDeque<u64>,
}

#[derive(Default)]
struct Table {
    locks: HashMap<Key, LockEntry>,
    /// What each owner holds, for `unlock_all`.
    held: HashMap<u64, HashSet<Key>>,
    /// What each waiting owner is blocked on, for deadlock detection.
    waiting: HashMap<u64, Key>,
}

pub struct LockManager {
    table: Mutex<Table>,
    cv: Condvar,
    timeout: Duration,
    deadlock_timeout: Duration,
}

impl LockManager {
    pub fn new(timeout_ms: u64, deadlock_timeout_ms: u64) -> Self {
        Self {
            table: Mutex::new(Table::default()),
            cv: Condvar::new(),
            timeout: Duration::from_millis(timeout_ms),
            deadlock_timeout: Duration::from_millis(deadlock_timeout_ms),
        }
    }

    /// Locks `(table, rid)` for `owner`, blocking until it is free or the
    /// timeout elapses. Re-entrant: re-locking a row we already hold succeeds.
    pub fn lock(&self, owner: u64, table: &str, rid: Rid) -> Result<()> {
        let key: Key = (table.to_string(), rid);
        let mut t = self.table.lock();
        match t.locks.get_mut(&key) {
            None => {
                t.locks.insert(key.clone(), LockEntry { holder: Some(owner), waiters: VecDeque::new() });
                t.held.entry(owner).or_default().insert(key);
                return Ok(());
            }
            Some(entry) => match entry.holder {
                Some(h) if h == owner => return Ok(()),
                None => {
                    entry.holder = Some(owner);
                    t.held.entry(owner).or_default().insert(key);
                    return Ok(());
                }
                Some(_) => entry.waiters.push_back(owner),
            },
        }
        t.waiting.insert(owner, key.clone());

        let start = Instant::now();
        let mut checked_deadlock = false;
        loop {
            if t.locks.get(&key).and_then(|e| e.holder) == Some(owner) {
                t.waiting.remove(&owner);
                return Ok(());
            }
            let elapsed = start.elapsed();
            if elapsed >= self.timeout {
                return Err(self.abort(&mut t, owner, &key, "lock wait timeout"));
            }
            if !checked_deadlock && elapsed >= self.deadlock_timeout {
                checked_deadlock = true;
                if self.in_cycle(&t, owner) {
                    return Err(self.abort(&mut t, owner, &key, "deadlock detected"));
                }
            }
            // Wake at the next deadlock check, not only at the full lock timeout,
            // so the graph is inspected on schedule.
            let remaining = self.timeout - elapsed;
            let wait = if checked_deadlock {
                remaining
            } else {
                remaining.min(self.deadlock_timeout)
            };
            self.cv.wait_for(&mut t, wait.max(Duration::from_millis(1)));
        }
    }

    /// Releases every lock `owner` holds, handing each to its next waiter.
    pub fn unlock_all(&self, owner: u64) {
        let mut t = self.table.lock();
        let Some(keys) = t.held.remove(&owner) else {
            return;
        };
        for key in keys {
            self.release_key(&mut t, &key);
        }
    }

    /// Whether `owner` currently holds `(table, rid)` (used by tests).
    #[cfg(test)]
    fn is_held(&self, owner: u64, table: &str, rid: Rid) -> bool {
        let t = self.table.lock();
        t.locks.get(&(table.to_string(), rid)).and_then(|e| e.holder) == Some(owner)
    }

    fn release_key(&self, t: &mut Table, key: &Key) {
        match t.locks.get_mut(key) {
            Some(entry) => match entry.waiters.pop_front() {
                Some(next) => {
                    entry.holder = Some(next);
                    t.held.entry(next).or_default().insert(key.clone());
                }
                None => {
                    t.locks.remove(key);
                }
            },
            None => return,
        }
        self.cv.notify_all();
    }

    /// Drops `owner` from the waiter queue after a timeout/deadlock.
    fn abort(&self, t: &mut Table, owner: u64, key: &Key, msg: &str) -> Error {
        if let Some(entry) = t.locks.get_mut(key) {
            entry.waiters.retain(|&w| w != owner);
        }
        t.waiting.remove(&owner);
        self.cv.notify_all();
        Error::Runtime(msg.into())
    }

    /// Whether `start`'s wait-for chain (waiter -> holder) returns to `start`.
    fn in_cycle(&self, t: &Table, start: u64) -> bool {
        let limit = t.waiting.len() + 1;
        let mut cur = start;
        for _ in 0..limit {
            let Some(key) = t.waiting.get(&cur) else {
                return false;
            };
            let Some(holder) = t.locks.get(key).and_then(|e| e.holder) else {
                return false;
            };
            if holder == start {
                return true;
            }
            cur = holder;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid(page: u32, slot: u16) -> Rid {
        Rid::new(page, slot)
    }

    #[test]
    fn reacquiring_a_held_row_succeeds() {
        let m = LockManager::new(1000, 100);
        m.lock(1, "t", rid(0, 0)).unwrap();
        m.lock(1, "t", rid(0, 0)).unwrap();
        assert!(m.is_held(1, "t", rid(0, 0)));
    }

    #[test]
    fn a_second_owner_times_out_on_a_held_row() {
        let m = LockManager::new(50, 1000);
        m.lock(1, "t", rid(0, 0)).unwrap();
        let err = m.lock(2, "t", rid(0, 0)).unwrap_err();
        assert!(err.to_string().contains("lock wait timeout"), "{err}");
        // and the timed-out owner holds nothing
        assert!(!m.is_held(2, "t", rid(0, 0)));
    }

    #[test]
    fn unlock_hands_the_row_to_the_next_waiter() {
        use std::sync::Arc;
        let m = Arc::new(LockManager::new(1000, 1000));
        m.lock(1, "t", rid(0, 0)).unwrap();
        let waiter = {
            let m = Arc::clone(&m);
            std::thread::spawn(move || {
                m.lock(2, "t", rid(0, 0)).unwrap();
                assert!(m.is_held(2, "t", rid(0, 0)));
                m.unlock_all(2);
            })
        };
        // give the waiter a moment to block, then release
        std::thread::sleep(Duration::from_millis(20));
        m.unlock_all(1);
        waiter.join().unwrap();
    }

    #[test]
    fn two_owners_waiting_on_each_others_rows_deadlock() {
        use std::sync::Arc;
        use std::sync::Barrier;
        let m = Arc::new(LockManager::new(300, 20));
        // owner 1 holds row a, owner 2 holds row b
        m.lock(1, "t", rid(1, 0)).unwrap();
        m.lock(2, "t", rid(2, 0)).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let result = |owner: u64, other: Rid| {
            let m = Arc::clone(&m);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                m.lock(owner, "t", other).map_err(|e| e.to_string())
            })
        };
        // Spawn both before joining: each waits on the barrier for the other.
        let a = result(1, rid(2, 0)); // owner 1 wants owner 2's row
        let b = result(2, rid(1, 0)); // owner 2 wants owner 1's row
        let a = a.join().unwrap();
        let b = b.join().unwrap();
        let deadlocks = [&a, &b]
            .iter()
            .filter(|r| matches!(r, Err(msg) if msg.contains("deadlock")))
            .count();
        assert!(deadlocks >= 1, "a cycle must abort at least one side: a={a:?} b={b:?}");
    }
}
