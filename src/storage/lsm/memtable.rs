//! In-memory sorted table: the write buffer an LSM flushes to SSTables.
//!
//! Keys are byte strings in ascending order, matching the index-key encoding.
//! A delete writes a tombstone rather than removing the key, so an older live
//! value in a lower level stays shadowed until compaction. The whole table is
//! guarded by one `RwLock`; the map is small, so this is enough for the first
//! version.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::RwLock;

/// A stored entry: a live value or a tombstone for a deleted key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemEntry {
    Value(Vec<u8>),
    Tombstone,
}

impl MemEntry {
    /// Whether this entry shadows older versions (a delete).
    pub fn is_tombstone(&self) -> bool {
        matches!(self, MemEntry::Tombstone)
    }

    /// The live bytes, or `None` for a tombstone.
    pub fn value(&self) -> Option<&[u8]> {
        match self {
            MemEntry::Value(v) => Some(v),
            MemEntry::Tombstone => None,
        }
    }

    /// Bytes this entry accounts for (key excluded, added by the caller).
    fn payload_len(&self) -> usize {
        match self {
            MemEntry::Value(v) => v.len(),
            MemEntry::Tombstone => 0,
        }
    }
}

/// A sorted, thread-safe write buffer.
pub struct MemTable {
    map: RwLock<BTreeMap<Vec<u8>, MemEntry>>,
    bytes: AtomicUsize,
}

impl Default for MemTable {
    fn default() -> Self {
        Self::new()
    }
}

impl MemTable {
    pub fn new() -> Self {
        Self { map: RwLock::new(BTreeMap::new()), bytes: AtomicUsize::new(0) }
    }

    /// Inserts or overwrites `key`.
    pub fn put(&self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) {
        self.write(key.into(), MemEntry::Value(value.into()));
    }

    /// Writes a tombstone so older versions of `key` are shadowed.
    pub fn delete(&self, key: impl Into<Vec<u8>>) {
        self.write(key.into(), MemEntry::Tombstone);
    }

    fn write(&self, key: Vec<u8>, entry: MemEntry) {
        let mut map = self.map.write();
        let old = map.insert(key.clone(), entry);
        self.account(key.len(), old.as_ref(), map.get(&key).expect("just inserted"));
    }

    /// Adjusts the byte counter for a replaced entry.
    fn account(&self, key_len: usize, old: Option<&MemEntry>, new: &MemEntry) {
        let old_bytes = old.map(|e| key_len + e.payload_len()).unwrap_or(0);
        let new_bytes = key_len + new.payload_len();
        if new_bytes >= old_bytes {
            self.bytes.fetch_add(new_bytes - old_bytes, Ordering::Relaxed);
        } else {
            self.bytes.fetch_sub(old_bytes - new_bytes, Ordering::Relaxed);
        }
    }

    /// The entry stored for `key`, including tombstones.
    pub fn get(&self, key: &[u8]) -> Option<MemEntry> {
        self.map.read().get(key).cloned()
    }

    /// All entries in ascending key order.
    pub fn iter(&self) -> Vec<(Vec<u8>, MemEntry)> {
        self.map.read().iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Entries whose key is in `[start, end)`, in ascending order.
    pub fn range(&self, start: &[u8], end: &[u8]) -> Vec<(Vec<u8>, MemEntry)> {
        self.map
            .read()
            .range(start.to_vec()..end.to_vec())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Approximate bytes held (keys plus live value bytes).
    pub fn approx_bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.map.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.read().is_empty()
    }
}
