//! Storage, catalog and rollback methods of [`Database`].

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use parking_lot::{RwLockReadGuard, RwLockWriteGuard};

use crate::catalog::meta::{encode_catalog, CatalogSnapshot};
use crate::catalog::{Catalog, HeapStore, IndexStore};
use crate::config::{EngineKind, PageLayout};
use crate::error::{Error, Result};
use crate::index::{encode_key, BTree};
use crate::storage::codec::encode_record;
use crate::storage::engine::{HeapEngine, TableStorage};
use crate::storage::lsm::engine::LsmEngine;
use crate::storage::{FileId, HeapFile, LobStore, Rid};
use crate::txn::trx::{TrxState, Undo};
use crate::value::Value;
use crate::wal::Record;

use super::{index_file_id, Database, LSM_BLOCK_SIZE};

impl Database {
    /// Buffer-pool lookup counters, for observability and cache-behavior tests.
    pub fn buffer_pool_stats(&self) -> crate::storage::PoolStats {
        self.pool.stats()
    }

    pub(crate) fn catalog(&self) -> RwLockReadGuard<'_, Catalog> {
        self.catalog.read()
    }

    /// Out-of-line storage for large string values.
    pub(crate) fn lobs(&self) -> &LobStore {
        &self.lobs
    }

    /// Deletes the large objects referenced by a record that is being
    /// physically removed (rollback or vacuum). Each object is owned by one
    /// version, so no other live version can share it.
    pub(super) fn free_lob_refs(&self, record: &[u8]) {
        for id in crate::storage::codec::collect_lob_ids(record) {
            let _ = self.lobs.delete(id);
        }
    }

    /// Strings longer than this are stored out-of-line.
    pub(crate) fn inline_lob_limit(&self) -> usize {
        self.config.storage.inline_lob_limit
    }

    /// Whether a table with `name` exists in this database.
    pub(crate) fn table_exists(&self, name: &str) -> bool {
        self.catalog().table(name).is_ok()
    }

    pub(crate) fn catalog_mut(&self) -> RwLockWriteGuard<'_, Catalog> {
        self.catalog.write()
    }

    /// The configured engine for newly created tables.
    pub(crate) fn default_engine(&self) -> EngineKind {
        self.config.storage.default_engine
    }

    /// The configured page layout for newly created heap tables.
    pub(crate) fn default_layout(&self) -> PageLayout {
        self.config.storage.page_layout
    }

    /// Creates the physical storage for a new table of the given engine kind.
    pub(crate) fn new_table_storage(
        &self,
        kind: EngineKind,
        layout: PageLayout,
    ) -> Result<(HeapStore, Arc<dyn TableStorage>)> {
        let file = self.next_table_file.fetch_add(1, Ordering::SeqCst);
        match kind {
            EngineKind::Heap => {
                let path = self.data_dir.join("tables").join(format!("{file:06}.dbf"));
                self.pool.create_file(file, &path)?;
                HeapFile::init_with_layout(&self.pool, file, layout)?;
                Ok((
                    HeapStore { file },
                    Arc::new(HeapEngine::with_layout(file, layout)),
                ))
            }
            EngineKind::Lsm => {
                let dir = self.data_dir.join("tables").join(format!("{file:06}.lsm"));
                let engine = LsmEngine::open_with_trigger(
                    &dir,
                    LSM_BLOCK_SIZE,
                    self.config.storage.lsm_compaction_trigger,
                )?;
                Ok((HeapStore { file }, Arc::new(engine)))
            }
        }
    }

    pub(crate) fn new_index_heap(&self, _name: &str) -> Result<IndexStore> {
        let no = self.next_index_file.fetch_add(1, Ordering::SeqCst);
        let file = index_file_id(no);
        let path = self.data_dir.join("indexes").join(format!("{no:06}.idxf"));
        self.pool.create_file(file, &path)?;
        BTree::init(&self.pool, file)?;
        Ok(IndexStore { file })
    }

    pub(crate) fn save_catalog(&self) -> Result<()> {
        let _serial = self.catalog_lock.lock();
        let snap = CatalogSnapshot {
            next_table_file: self.next_table_file.load(Ordering::SeqCst),
            next_index_file: self.next_index_file.load(Ordering::SeqCst),
            next_trx_id: self.trx.next_id(),
            clog_base: self.trx.clog_base(),
            committed_trxs: self.trx.committed_ids(),
            tables: self.catalog().table_metas(),
            indexes: self.catalog().index_metas(),
            views: self.catalog().view_metas(),
        };
        let bytes = encode_catalog(&snap);
        // Write a temp file, fsync it, then rename over catalog.bin. Rename is
        // atomic, so a crash leaves either the old catalog or the new one,
        // never a half-written one (same pattern as the LSM manifest).
        let tmp = self.data_dir.join("catalog.tmp");
        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&tmp)
                .map_err(|e| Error::Runtime(format!("cannot create catalog temp: {e}")))?;
            file.write_all(&bytes)
                .map_err(|e| Error::Runtime(format!("cannot write catalog: {e}")))?;
            file.sync_all()
                .map_err(|e| Error::Runtime(format!("cannot sync catalog: {e}")))?;
        }
        std::fs::rename(&tmp, self.data_dir.join("catalog.bin"))
            .map_err(|e| Error::Runtime(format!("cannot replace catalog: {e}")))
    }

    /// Drops a table: catalog first (durability), then its heap and index
    /// files. A crash in between leaves harmless orphan files behind.
    pub(crate) fn drop_table(&self, name: &str) -> Result<()> {
        // free every large object the table owns before it disappears
        if let Ok(records) = self.store_scan_raw(name) {
            for (_, record) in &records {
                self.free_lob_refs(record);
            }
        }
        let dropped = self.catalog_mut().drop_table(name)?;
        self.save_catalog()?;
        if dropped.engine == EngineKind::Lsm {
            let dir = self.data_dir.join("tables").join(format!("{:06}.lsm", dropped.heap_file));
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(Error::Runtime(format!(
                        "cannot delete {}: {e}",
                        dir.display()
                    )))
                }
            }
        } else {
            let path = self.pool.close_file(dropped.heap_file)?;
            remove_if_exists(&path)?;
        }
        for file in dropped.index_files {
            let path = self.pool.close_file(file)?;
            remove_if_exists(&path)?;
        }
        Ok(())
    }

    /// (column index, index file) pairs for every index on `table`.
    pub(crate) fn index_ops(&self, table: &str) -> Result<Vec<(usize, FileId)>> {
        let catalog = self.catalog();
        let schema = &catalog.table(table)?.schema;
        Ok(catalog
            .indexes_for(table)
            .into_iter()
            .map(|ix| {
                let ci = schema
                    .index_of(&ix.column)
                    .expect("index column validated at creation");
                (ci, ix.store.file)
            })
            .collect())
    }

    /// Raw versioned records; callers decode and apply visibility.
    pub(crate) fn store_scan_raw(&self, name: &str) -> Result<Vec<(Rid, Vec<u8>)>> {
        let engine = self.catalog().table(name)?.engine();
        let mut scanner = engine.scan(&self.pool)?;
        let mut out = Vec::new();
        while let Some(row) = scanner.next(&self.pool)? {
            out.push(row);
        }
        Ok(out)
    }

    /// Appends a `Begin` frame to the WAL the first time this transaction
    /// writes, so recovery's `max_trx_id` covers its id even if it never
    /// commits. Without it, an aborted transaction whose dirty pages were
    /// evicted could have its id reused after a crash, resurrecting its orphan
    /// versions (they would look committed).
    fn log_trx_begin(&self, trx: &mut TrxState) -> Result<()> {
        if !trx.wal_began {
            self.wal.append(trx.id, &Record::Begin)?;
            trx.wal_began = true;
        }
        Ok(())
    }

    pub(crate) fn store_insert(
        &self,
        name: &str,
        row: Vec<Value>,
        trx: &mut TrxState,
    ) -> Result<Rid> {
        let (file, engine) = {
            let catalog = self.catalog();
            let t = catalog.table(name)?;
            (t.heap.file, t.engine())
        };
        let creator = trx.id;
        self.log_trx_begin(trx)?;
        self.note_write(creator, name);
        let data = encode_record(creator, 0, 0, &row, &self.lobs, self.inline_lob_limit())?;
        let rid = engine.insert(&self.pool, &data)?;
        // Record the undo as soon as the row exists so that a later failure in
        // the WAL or index steps is still undone by the enclosing transaction.
        trx.undo.push(Undo::Insert { table: name.to_string(), rid, row: row.clone() });
        crate::wal::encode_frame_into(
            &mut trx.wal,
            creator,
            &Record::Insert { file, rid, record: data },
        );
        for (ci, ix_file) in self.index_ops(name)? {
            let key = encode_key(&row[ci])?;
            BTree::at(ix_file).insert(&self.pool, &key, rid)?;
        }
        Ok(rid)
    }

    /// MVCC delete: mark records with the deleter's trx id (index untouched,
    /// stale entries are filtered by visibility on read).
    pub(crate) fn store_delete_mark(
        &self,
        name: &str,
        rids: &[Rid],
        trx: &mut TrxState,
    ) -> Result<()> {
        let (file, engine) = {
            let catalog = self.catalog();
            let t = catalog.table(name)?;
            (t.heap.file, t.engine())
        };
        let deleter = trx.id;
        self.log_trx_begin(trx)?;
        self.note_write(deleter, name);
        for rid in rids {
            // serialize writers of the same row; different rows proceed
            self.locks.lock(deleter, name, *rid)?;
            // read committed: the row changed under us, so restart the statement
            // (EPQ) with a fresh snapshot rather than abort.
            if self.isolation() == crate::config::Isolation::ReadCommitted
                && self.row_was_concurrently_modified(*rid, &*engine, trx)?
            {
                return Err(Error::Retry);
            }
            let (prev_deleter, prev_next_rid) = engine.delete_mark(&self.pool, *rid, deleter, 0)?;
            trx.undo.push(Undo::DeleteMark {
                table: name.to_string(),
                rid: *rid,
                prev_deleter,
                prev_next_rid,
            });
            crate::wal::encode_frame_into(
                &mut trx.wal,
                deleter,
                &Record::DeleteMark { file, rid: *rid, deleter, next_rid: 0 },
            );
        }
        Ok(())
    }

    /// MVCC update: delete-mark the old version, insert a new one. Index
    /// entries for the new version are added; old entries stay so older
    /// snapshots can still find them (filtered by visibility on read).
    pub(crate) fn store_update_versions(
        &self,
        name: &str,
        updates: &[(Rid, Vec<Value>)],
        trx: &mut TrxState,
    ) -> Result<()> {
        let (file, engine) = {
            let catalog = self.catalog();
            let t = catalog.table(name)?;
            (t.heap.file, t.engine())
        };
        let trx_id = trx.id;
        self.log_trx_begin(trx)?;
        self.note_write(trx_id, name);
        let ops = self.index_ops(name)?;
        for (rid, new_row) in updates {
            // serialize writers of the same row; different rows proceed
            self.locks.lock(trx_id, name, *rid)?;
            // read committed: the row changed under us, so restart the statement
            // (EPQ) with a fresh snapshot rather than abort.
            if self.isolation() == crate::config::Isolation::ReadCommitted
                && self.row_was_concurrently_modified(*rid, &*engine, trx)?
            {
                return Err(Error::Retry);
            }
            let data = encode_record(trx_id, 0, 0, new_row, &self.lobs, self.inline_lob_limit())?;
            let new_rid = engine.insert(&self.pool, &data)?;
            crate::wal::encode_frame_into(
                &mut trx.wal,
                trx_id,
                &Record::Insert { file, rid: new_rid, record: data },
            );
            // link the old version forward to the new one (PG's t_ctid)
            let next_rid = crate::storage::codec::pack_rid(new_rid.page_no, new_rid.slot);
            let (prev_deleter, prev_next_rid) =
                engine.delete_mark(&self.pool, *rid, trx_id, next_rid)?;
            // Record the undo before the index step: if an index insert fails,
            // the transaction's rollback must still remove the new version and
            // restore the old one, or both would stay live.
            trx.undo.push(Undo::Update {
                table: name.to_string(),
                old_rid: *rid,
                new_rid,
                new_row: new_row.clone(),
                prev_deleter,
                prev_next_rid,
            });
            crate::wal::encode_frame_into(
                &mut trx.wal,
                trx_id,
                &Record::DeleteMark { file, rid: *rid, deleter: trx_id, next_rid },
            );
            for (ci, ix_file) in &ops {
                let key = encode_key(&new_row[*ci])?;
                BTree::at(*ix_file).insert(&self.pool, &key, new_rid)?;
            }
        }
        Ok(())
    }

    pub(super) fn rollback_trx(&self, trx: &mut TrxState) -> Result<()> {
        self.rollback_trx_to(trx, 0, 0)
    }

    /// Undoes only the undo entries above `undo_mark` and drops the redo frames
    /// buffered after `wal_mark`. Used for a statement-level rollback inside an
    /// explicit transaction, so earlier statements survive and their buffered
    /// frames stay.
    pub(crate) fn rollback_trx_to(
        &self,
        trx: &mut TrxState,
        undo_mark: usize,
        wal_mark: usize,
    ) -> Result<()> {
        // Reverse the writes *before* releasing locks: while the undo runs the
        // transaction's marks are still on the rows, and another writer must not
        // be able to observe (and build on) them.
        let undone = self.undo_to(trx, undo_mark, wal_mark);
        if undo_mark == 0 {
            self.locks.unlock_all(trx.id);
        }
        undone
    }

    /// Undoes the entries above `undo_mark` and drops the redo frames buffered
    /// after `wal_mark` **without releasing locks**. An EPQ restart uses this so
    /// the row lock it just won is held across the retry: releasing it would let
    /// a competing writer steal the row and starve the retry.
    pub(crate) fn rollback_statement(
        &self,
        trx: &mut TrxState,
        undo_mark: usize,
        wal_mark: usize,
    ) -> Result<()> {
        self.undo_to(trx, undo_mark, wal_mark)
    }

    fn undo_to(&self, trx: &mut TrxState, undo_mark: usize, wal_mark: usize) -> Result<()> {
        trx.wal.truncate(wal_mark);
        while trx.undo.len() > undo_mark {
            let undo = trx.undo.pop().expect("len > mark checked");
            self.undo_one(&undo)?;
        }
        Ok(())
    }

    fn undo_one(&self, undo: &Undo) -> Result<()> {
        match undo {
            Undo::Insert { table, rid, row } => {
                let engine = self.catalog().table(table)?.engine();
                if let Ok(record) = engine.get(&self.pool, *rid) {
                    self.free_lob_refs(&record);
                }
                engine.delete(&self.pool, *rid)?;
                for (ci, ix_file) in self.index_ops(table)? {
                    let key = encode_key(&row[ci])?;
                    BTree::at(ix_file).delete(&self.pool, &key, *rid)?;
                }
            }
            Undo::DeleteMark { table, rid, prev_deleter, prev_next_rid } => {
                let engine = self.catalog().table(table)?.engine();
                engine.delete_mark(&self.pool, *rid, *prev_deleter, *prev_next_rid)?;
            }
            Undo::Update { table, old_rid, new_rid, new_row, prev_deleter, prev_next_rid } => {
                let engine = self.catalog().table(table)?.engine();
                if let Ok(record) = engine.get(&self.pool, *new_rid) {
                    self.free_lob_refs(&record);
                }
                engine.delete(&self.pool, *new_rid)?;
                for (ci, ix_file) in self.index_ops(table)? {
                    let key = encode_key(&new_row[ci])?;
                    BTree::at(ix_file).delete(&self.pool, &key, *new_rid)?;
                }
                engine.delete_mark(&self.pool, *old_rid, *prev_deleter, *prev_next_rid)?;
            }
        }
        Ok(())
    }
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Runtime(format!("cannot delete file {}: {e}", path.display()))),
    }
}
