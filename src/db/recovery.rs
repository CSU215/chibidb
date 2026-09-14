//! WAL replay and index-rebuild methods of [`Database`].

use std::collections::HashSet;
use std::sync::Arc;

use crate::config::EngineKind;
use crate::error::{Error, Result};
use crate::index::{encode_key, BTree};
use crate::storage::engine::TableStorage;
use crate::wal::{self, Record};

use super::Database;

/// WAL-replay routing: file number to its engine kind and handle.
type StorageMap = std::collections::HashMap<u32, (EngineKind, Arc<dyn TableStorage>)>;

impl Database {
    /// Replays committed WAL records into the buffer pool (they reach the
    /// disk with the next flush) and rebuilds indexes of touched tables.
    /// `touched` accumulates heap file numbers that must have their indexes
    /// rebuilt; it may arrive pre-seeded with repaired index files.
    pub(super) fn recover_from_wal(
        &self,
        plan: &wal::RecoveryPlan,
        touched: &mut HashSet<u32>,
    ) -> Result<()> {
        // Route each redo record to the engine that owns its table; every
        // engine implements `insert_at`/`delete_mark` idempotently.
        let storage: StorageMap = {
            let catalog = self.catalog();
            let mut storage = StorageMap::new();
            for (file_no, _) in catalog.heap_files() {
                if let Some(entry) = catalog.storage_for_file_no(file_no) {
                    storage.insert(file_no, entry);
                }
            }
            storage
        };
        for (_, _, records) in &plan.committed {
            for rec in records {
                match rec {
                    Record::Insert { file_no, rid, record } => {
                        // records of dropped tables (file no longer in the
                        // catalog) are stale and skipped
                        let Some((_kind, engine)) = storage.get(file_no) else { continue };
                        // Rebuild the table's indexes for any committed record,
                        // even if the heap page already reflects it: the derived
                        // index page may not have reached disk.
                        touched.insert(*file_no);
                        engine.insert_at(&self.pool, *rid, record)?;
                    }
                    Record::DeleteMark { file_no, rid, deleter, next_rid } => {
                        let Some((kind, engine)) = storage.get(file_no) else { continue };
                        touched.insert(*file_no);
                        // a heap record past the last allocated page was never
                        // written, so there is nothing to mark
                        if *kind == EngineKind::Heap
                            && self.pool.page_count(engine.file_id())? <= rid.page_no
                        {
                            continue;
                        }
                        if let Ok(bytes) = engine.get(&self.pool, *rid)
                            && bytes.len() >= crate::storage::codec::RECORD_HEADER
                            && u64::from_le_bytes(bytes[8..16].try_into().unwrap()) == 0
                        {
                            engine.delete_mark(&self.pool, *rid, *deleter, *next_rid)?;
                        }
                    }
                    Record::Commit => {}
                }
            }
        }
        for file_no in std::mem::take(touched) {
            self.rebuild_indexes(file_no)?;
        }
        Ok(())
    }

    /// Rebuilds every index of the table owning heap file `file_no` from the
    /// heap contents. Index pages are derived data and a crash may have lost
    /// unflushed ones.
    fn rebuild_indexes(&self, file_no: u32) -> Result<()> {
        let table = self
            .catalog()
            .table_metas()
            .into_iter()
            .find(|m| m.file_no == file_no)
            .map(|m| m.name)
            .ok_or_else(|| Error::Runtime(format!("no table owns file {file_no}")))?;
        let ops = self.index_ops(&table)?;
        for (_, ix_file) in &ops {
            self.pool.discard_file(*ix_file);
            self.pool.truncate_file(*ix_file)?;
            BTree::init(&self.pool, *ix_file)?;
        }
        for (rid, rec) in self.store_scan_raw(&table)? {
            let (_, _, row) = crate::storage::codec::decode_record(&rec, &self.lobs)?;
            for (ci, ix_file) in &ops {
                let key = encode_key(&row[*ci])?;
                BTree::at(*ix_file).insert(&self.pool, &key, rid)?;
            }
        }
        Ok(())
    }
}
