//! An LSM-backed [`TableStorage`].
//!
//! Rows are keyed by an internal, monotonically increasing id that maps to a
//! [`Rid`], so the versioned-record format (`creator`, `deleter`, row bytes)
//! and every caller of the storage seam stay exactly as for the heap. The LSM
//! store is only a different physical layout for the same records.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::storage::buffer::BufferPool;
use crate::storage::engine::{RowScanner, TableEngine, TableStorage};
use crate::storage::heap::Rid;
use crate::storage::lsm::persist::PersistentLsm;
use crate::storage::lsm::store::MergeScanner;
use crate::storage::page::FileId;
use crate::{Error, Result};

/// Reserved file id reported for LSM tables (they have no heap file).
pub const LSM_FILE_ID: FileId = u32::MAX;

pub struct LsmEngine {
    inner: Mutex<PersistentLsm>,
    next_rid: AtomicU64,
}

impl LsmEngine {
    /// Opens (or creates) the LSM directory and resumes the row-id counter
    /// after the largest key already stored.
    pub fn open(dir: &Path, block_size: usize) -> Result<Self> {
        Self::open_with_trigger(
            dir,
            block_size,
            crate::storage::lsm::persist::DEFAULT_COMPACTION_TRIGGER,
        )
    }

    /// Like [`LsmEngine::open`] but with an explicit compaction trigger.
    pub fn open_with_trigger(
        dir: &Path,
        block_size: usize,
        compaction_trigger: usize,
    ) -> Result<Self> {
        let lsm = PersistentLsm::open_with_trigger(dir, block_size, compaction_trigger)?;
        let mut max_id = 0u64;
        for (key, _) in lsm.iter()? {
            if let Ok(bytes) = <[u8; 8]>::try_from(key.as_slice()) {
                max_id = max_id.max(u64::from_be_bytes(bytes));
            }
        }
        Ok(Self { inner: Mutex::new(lsm), next_rid: AtomicU64::new(max_id + 1) })
    }

    /// Flushes the memtable to a new SSTable.
    pub fn flush(&self) -> Result<()> {
        self.inner.lock().flush()
    }

    /// Merges every SSTable into one.
    pub fn compact(&self) -> Result<()> {
        self.inner.lock().compact()
    }

    /// Writes a version at an exact row id during WAL replay, keeping the id
    /// counter ahead of replayed ids so later inserts do not collide.
    pub fn insert_at(&self, rid: Rid, record: &[u8]) -> Result<()> {
        let id = ((rid.page_no as u64) << 16) | rid.slot as u64;
        let mut current = self.next_rid.load(Ordering::SeqCst);
        while current <= id {
            match self.next_rid.compare_exchange(
                current,
                id + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        self.inner.lock().put(rid_key(rid), record.to_vec());
        Ok(())
    }

    pub fn num_sstables(&self) -> usize {
        self.inner.lock().num_sstables()
    }
}

impl std::fmt::Debug for LsmEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LsmEngine").finish_non_exhaustive()
    }
}

impl TableEngine for LsmEngine {
    fn scan(&self, _bp: &BufferPool) -> Result<Box<dyn RowScanner>> {
        // snapshot under the lock, then stream without holding it; `snapshot`
        // already yields the tables newest first
        let (mem, sstables) = {
            let lsm = self.inner.lock();
            lsm.snapshot()
        };
        let merge = MergeScanner::new(mem, sstables)?;
        Ok(Box::new(LsmScanner { merge }))
    }

    fn get(&self, _bp: &BufferPool, rid: Rid) -> Result<Vec<u8>> {
        self.try_get(_bp, rid)?
            .ok_or_else(|| Error::Runtime(format!("no record at {rid:?}")))
    }

    fn try_get(&self, _bp: &BufferPool, rid: Rid) -> Result<Option<Vec<u8>>> {
        self.inner.lock().get(&rid_key(rid))
    }
}

impl TableStorage for LsmEngine {
    fn insert(&self, _bp: &BufferPool, record: &[u8]) -> Result<Rid> {
        let id = self.next_rid.fetch_add(1, Ordering::SeqCst);
        let rid = id_to_rid(id);
        self.inner.lock().put(rid_key(rid), record.to_vec());
        Ok(rid)
    }

    fn delete(&self, _bp: &BufferPool, rid: Rid) -> Result<()> {
        self.inner.lock().delete(rid_key(rid));
        Ok(())
    }

    fn delete_mark(&self, _bp: &BufferPool, rid: Rid, deleter: u64, next_rid: u64)
    -> Result<(u64, u64)> {
        let mut lsm = self.inner.lock();
        let key = rid_key(rid);
        let mut record = lsm
            .get(&key)?
            .ok_or_else(|| Error::Runtime(format!("no record at {rid:?}")))?;
        if record.len() < crate::storage::codec::RECORD_HEADER {
            return Err(Error::Runtime("record lacks mvcc fields".into()));
        }
        let previous = u64::from_le_bytes(record[8..16].try_into().unwrap());
        let prev_next = u64::from_le_bytes(record[16..24].try_into().unwrap());
        record[8..16].copy_from_slice(&deleter.to_le_bytes());
        record[16..24].copy_from_slice(&next_rid.to_le_bytes());
        lsm.put(key, record);
        Ok((previous, prev_next))
    }

    fn file_id(&self) -> FileId {
        LSM_FILE_ID
    }

    fn insert_at(&self, _bp: &BufferPool, rid: Rid, record: &[u8]) -> Result<()> {
        LsmEngine::insert_at(self, rid, record)
    }

    fn flush(&self) -> Result<()> {
        LsmEngine::flush(self)
    }
}

/// Streams merged key/value pairs as `(Rid, record)` rows.
struct LsmScanner {
    merge: MergeScanner,
}

impl RowScanner for LsmScanner {
    fn next(&mut self, _bp: &BufferPool) -> Result<Option<(Rid, Vec<u8>)>> {
        while let Some((key, value)) = self.merge.next_entry()? {
            if let Ok(bytes) = <[u8; 8]>::try_from(key.as_slice()) {
                return Ok(Some((id_to_rid(u64::from_be_bytes(bytes)), value)));
            }
        }
        Ok(None)
    }
}

/// Packs a row id into a `Rid` (16 bits of slot per page number).
fn id_to_rid(id: u64) -> Rid {
    Rid { page_no: (id >> 16) as u32, slot: (id & 0xffff) as u16 }
}

/// The LSM key for a row: big-endian so lexicographic order follows the id.
fn rid_key(rid: Rid) -> Vec<u8> {
    let id = ((rid.page_no as u64) << 16) | rid.slot as u64;
    id.to_be_bytes().to_vec()
}
