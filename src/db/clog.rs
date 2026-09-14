//! Commit status, a small PostgreSQL-style clog.
//!
//! xids are dense and monotonic, so a per-xid bitmap answers "did this
//! transaction commit?" in O(1). Reads take a shared read lock just long
//! enough to index a word; commits flip an atomic bit, so they never block
//! readers, and the bitmap only grows once per chunk (every `CHUNK_BITS` xids).
//! Unlike the old `HashSet<u32>`, a snapshot never copies it — a transaction
//! holds an `Arc<CommitStatus>` and queries it. Truncating the prefix below a
//! vacuum horizon is a later step.

use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;

/// Bits per word.
const BITS: usize = 64;
/// Words added per growth step (64 KiB of xid space).
const WORDS_PER_CHUNK: usize = 1024;

#[derive(Default)]
pub struct CommitStatus {
    /// Bit `xid % 64` of word `xid / 64`.
    words: RwLock<Vec<AtomicU64>>,
}

impl CommitStatus {
    pub fn new() -> Self {
        Self { words: RwLock::new(Vec::new()) }
    }

    /// Records that `xid` committed. Idempotent.
    pub fn mark_committed(&self, xid: u64) {
        let word = xid as usize / BITS;
        let mask = 1u64 << (xid as usize % BITS);
        {
            let words = self.words.read();
            if word < words.len() {
                words[word].fetch_or(mask, Ordering::Release);
                return;
            }
        }
        let mut words = self.words.write();
        if word >= words.len() {
            let chunk = (word / WORDS_PER_CHUNK + 1) * WORDS_PER_CHUNK;
            words.resize_with(chunk, || AtomicU64::new(0));
        }
        words[word].fetch_or(mask, Ordering::Release);
    }

    /// Whether `xid` committed. xid 0 (the read-only sentinel) never has.
    pub fn is_committed(&self, xid: u64) -> bool {
        let word = xid as usize / BITS;
        let bit = xid as usize % BITS;
        let words = self.words.read();
        match words.get(word) {
            Some(w) => w.load(Ordering::Acquire) & (1u64 << bit) != 0,
            None => false,
        }
    }

    /// Every committed xid, ascending. O(bitmap); for catalogs/checkpoints.
    pub fn ids(&self) -> Vec<u64> {
        let words = self.words.read();
        let mut out = Vec::new();
        for (w, word) in words.iter().enumerate() {
            let mut bits = word.load(Ordering::Relaxed);
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                out.push((w * BITS + bit) as u64);
            }
        }
        out
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
}
