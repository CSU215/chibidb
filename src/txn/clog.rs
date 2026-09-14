//! Commit status, a small PostgreSQL-style clog.
//!
//! xids are dense and monotonic, so a per-xid bitmap answers "did this
//! transaction commit?" in O(1). Reads take a shared read lock just long
//! enough to index a word; commits flip an atomic bit, so they never block
//! readers, and the bitmap only grows once per chunk (every `CHUNK_BITS` xids).
//! A transaction holds an `Arc<CommitStatus>` and queries it.
//!
//! `base` is a vacuum horizon: every xid below it is treated as committed.
//! Vacuum first physically removes versions whose creator never committed and
//! clears delete marks left by a transaction that never committed, so no live
//! version can still need the real status of an xid below the horizon. Once it
//! has, `advance_base` drops the bitmap prefix and the clog stays bounded.

use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;

/// Bits per word.
const BITS: usize = 64;
/// Words added per growth step (64 KiB of xid space).
const WORDS_PER_CHUNK: usize = 1024;

#[derive(Default)]
struct ClogInner {
    /// xids below this are frozen as committed (vacuum horizon).
    base: u64,
    /// Bit `(xid - base) % 64` of word `(xid - base) / 64`.
    words: Vec<AtomicU64>,
}

#[derive(Default)]
pub struct CommitStatus {
    inner: RwLock<ClogInner>,
}

/// Sets the bit for `xid` in `words`, growing the chunked array as needed.
fn set_bit(inner: &mut ClogInner, xid: u64) {
    let idx = (xid - inner.base) as usize;
    let word = idx / BITS;
    if word >= inner.words.len() {
        let chunk = (word / WORDS_PER_CHUNK + 1) * WORDS_PER_CHUNK;
        inner.words.resize_with(chunk, || AtomicU64::new(0));
    }
    inner.words[word].fetch_or(1u64 << (idx % BITS), Ordering::Release);
}

impl CommitStatus {
    pub fn new() -> Self {
        Self { inner: RwLock::new(ClogInner::default()) }
    }

    /// Records that `xid` committed. Idempotent, and a no-op for a frozen xid.
    pub fn mark_committed(&self, xid: u64) {
        {
            let inner = self.inner.read();
            if xid < inner.base {
                return;
            }
            let idx = (xid - inner.base) as usize;
            let word = idx / BITS;
            if word < inner.words.len() {
                inner.words[word].fetch_or(1u64 << (idx % BITS), Ordering::Release);
                return;
            }
        }
        let mut inner = self.inner.write();
        if xid < inner.base {
            return;
        }
        set_bit(&mut inner, xid);
    }

    /// Whether `xid` committed. xid 0 (the read-only sentinel) never has, and
    /// any xid below the vacuum horizon is frozen as committed.
    pub fn is_committed(&self, xid: u64) -> bool {
        if xid == 0 {
            return false;
        }
        let inner = self.inner.read();
        if xid < inner.base {
            return true;
        }
        let idx = (xid - inner.base) as usize;
        let word = idx / BITS;
        match inner.words.get(word) {
            Some(w) => w.load(Ordering::Acquire) & (1u64 << (idx % BITS)) != 0,
            None => false,
        }
    }

    /// Every committed xid at or above the horizon, ascending. O(bitmap).
    pub fn ids(&self) -> Vec<u64> {
        let inner = self.inner.read();
        let mut out = Vec::new();
        for (w, word) in inner.words.iter().enumerate() {
            let mut bits = word.load(Ordering::Relaxed);
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                out.push(inner.base + (w * BITS + bit) as u64);
            }
        }
        out
    }

    /// The current vacuum horizon.
    pub fn base(&self) -> u64 {
        self.inner.read().base
    }

    /// Advances the horizon to `new_base` (monotonic), dropping the prefix.
    /// The caller must have made below-horizon status recoverable as
    /// "committed": see [`CommitStatus`] module docs. Returns the horizon.
    pub fn advance_base(&self, new_base: u64) -> u64 {
        let mut inner = self.inner.write();
        if new_base <= inner.base {
            return inner.base;
        }
        let base = inner.base;
        let mut kept: Vec<u64> = Vec::new();
        for (w, word) in inner.words.iter().enumerate() {
            let mut bits = word.load(Ordering::Relaxed);
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let xid = base + (w * BITS + bit) as u64;
                if xid >= new_base {
                    kept.push(xid);
                }
            }
        }
        inner.base = new_base;
        inner.words = Vec::new();
        for xid in kept {
            set_bit(&mut inner, xid);
        }
        inner.base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_and_reads_back_across_chunks() {
        let clog = CommitStatus::new();
        let across = (BITS * WORDS_PER_CHUNK) as u64 + 5; // forces a second chunk
        assert!(!clog.is_committed(1));
        clog.mark_committed(1);
        clog.mark_committed(across);
        clog.mark_committed(1); // idempotent
        assert!(clog.is_committed(1));
        assert!(clog.is_committed(across));
        assert!(!clog.is_committed(2));
        assert_eq!(clog.ids(), vec![1, across]);
    }

    #[test]
    fn xid_zero_is_never_committed() {
        let clog = CommitStatus::new();
        clog.mark_committed(1);
        assert!(!clog.is_committed(0));
    }

    #[test]
    fn advancing_the_horizon_freezes_the_prefix_and_drops_its_bits() {
        let clog = CommitStatus::new();
        clog.mark_committed(3);
        clog.mark_committed(9);
        assert_eq!(clog.base(), 0);
        assert_eq!(clog.advance_base(5), 5);
        // 3 is below the horizon: frozen as committed even though it was not marked
        assert!(clog.is_committed(3));
        assert!(clog.is_committed(4));
        // 9 survives the compaction and stays committed
        assert!(clog.is_committed(9));
        // a never-committed xid above the horizon is still not committed
        assert!(!clog.is_committed(6));
        assert_eq!(clog.ids(), vec![9]);
        assert_eq!(clog.base(), 5);
        // the horizon only moves forward
        assert_eq!(clog.advance_base(2), 5);
    }
}
