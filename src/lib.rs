pub mod ast;
pub mod catalog;
pub mod client;
pub mod datetime;
mod error;
pub mod exec;
pub mod index;
pub mod lexer;
pub mod parser;
pub mod render;
mod repl;
pub mod result;
pub mod server;
pub mod storage;
pub mod trx;
pub mod value;
pub mod wal;
pub mod wire;

pub use error::{Error, Result};
pub use repl::run_repl;
pub use result::ResultSet;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::catalog::meta::{decode_catalog, encode_catalog, CatalogSnapshot};
use crate::catalog::{Catalog, ColumnDesc, HeapStore, IndexStore, Schema};
use crate::index::{encode_key, BTree};
use crate::storage::codec::encode_record;
use crate::storage::slotted::{page_get, page_put_at};
use crate::storage::{BufferPool, DiskManager, FileId, HeapFile, Rid};
use crate::trx::{TrxState, Undo};
use crate::value::Value;
use crate::wal::{Record, Wal};

pub use crate::trx::Session;

pub const BUFFER_POOL_FRAMES: usize = 64;

pub struct Database {
    catalog: Catalog,
    pool: BufferPool,
    wal: Wal,
    data_dir: PathBuf,
    next_table_file: u32,
    next_index_file: u32,
    next_trx_id: u32,
    committed_trxs: HashSet<u32>,
    _temp: Option<tempfile::TempDir>,
}

impl Database {
    /// A throwaway database in an automatically-cleaned temporary directory.
    pub fn open_in_memory() -> Result<Self> {
        let temp = tempfile::tempdir()
            .map_err(|e| Error::Runtime(format!("cannot create temp dir: {e}")))?;
        let db = Self::open(temp.path())?;
        Ok(Self { _temp: Some(temp), ..db })
    }

    pub fn open(path: &Path) -> Result<Self> {
        let tables_dir = path.join("tables");
        let indexes_dir = path.join("indexes");
        std::fs::create_dir_all(&tables_dir).map_err(dir_err(&tables_dir))?;
        std::fs::create_dir_all(&indexes_dir).map_err(dir_err(&indexes_dir))?;
        let mut pool = BufferPool::new(DiskManager::new(), BUFFER_POOL_FRAMES);
        let mut catalog = Catalog::default();
        let mut next_table_file = 0;
        let mut next_index_file = 0;
        let mut next_trx_id = 1;
        let mut committed_trxs: HashSet<u32> = HashSet::new();
        // tables whose index file headers were rebuilt after a crash; their
        // contents must be re-derived from the heap
        let mut repaired_index_tables: Vec<String> = Vec::new();

        let catalog_path = path.join("catalog.bin");
        if catalog_path.exists() {
            let bytes = std::fs::read(&catalog_path)
                .map_err(|e| Error::Runtime(format!("cannot read catalog: {e}")))?;
            let snap = decode_catalog(&bytes)?;
            for meta in &snap.tables {
                let fpath = tables_dir.join(format!("{:06}.dbf", meta.file_no));
                let file = pool.open_file(&fpath)?;
                HeapFile::open_or_repair(&mut pool, file)?;
                let schema = Schema {
                    columns: meta
                        .columns
                        .iter()
                        .map(|(name, dtype)| ColumnDesc {
                            owner: None,
                            name: name.clone(),
                            dtype: *dtype,
                        })
                        .collect(),
                };
                catalog.create_table(
                    &meta.name,
                    schema,
                    HeapStore { file, file_no: meta.file_no },
                )?;
            }
            for ix in &snap.indexes {
                let fpath = indexes_dir.join(format!("{:06}.idxf", ix.file_no));
                let file = pool.open_file(&fpath)?;
                if BTree::open_or_repair(&mut pool, file)? {
                    repaired_index_tables.push(ix.table.clone());
                }
                let schema = &catalog.table(&ix.table)?.schema;
                if schema.index_of(&ix.column).is_none() {
                    return Err(Error::Runtime(format!(
                        "corrupt catalog: index {} on unknown column {}.{}",
                        ix.name, ix.table, ix.column
                    )));
                }
                catalog.create_index(
                    &ix.name,
                    ix.table.clone(),
                    ix.column.clone(),
                    IndexStore { file, file_no: ix.file_no },
                )?;
            }
            next_table_file = snap.next_table_file;
            next_index_file = snap.next_index_file;
            next_trx_id = snap.next_trx_id;
            committed_trxs = snap.committed_trxs.into_iter().collect();
        }

        // WAL recovery: redo the committed transactions whose data pages
        // never reached the disk, then repair derived structures.
        let wal_path = path.join("wal.bin");
        let wal_bytes = match std::fs::read(&wal_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(Error::Runtime(format!("cannot read wal: {e}"))),
        };
        let plan = wal::plan_recovery(&wal_bytes);

        let mut touched: HashSet<u32> = HashSet::new();
        for table in &repaired_index_tables {
            touched.insert(catalog.table(table)?.heap.file_no);
        }

        let mut db = Self {
            catalog,
            pool,
            wal: Wal::open(&wal_path)?,
            data_dir: path.to_path_buf(),
            next_table_file,
            next_index_file,
            next_trx_id,
            committed_trxs,
            _temp: None,
        };
        db.recover_from_wal(&plan, &mut touched)?;

        // never hand a crashed transaction's id to a new transaction
        if plan.max_trx_id >= db.next_trx_id {
            db.next_trx_id = plan.max_trx_id.saturating_add(1);
        }
        let committed_repaired =
            plan.committed_ids.iter().any(|id| !db.committed_trxs.contains(id));
        if committed_repaired {
            db.committed_trxs.extend(plan.committed_ids);
        }
        // persist the repaired committed set so it survives a second crash
        // even if nothing else is written in this session
        if committed_repaired {
            db.save_catalog()?;
        }
        Ok(db)
    }

    pub fn flush(&mut self) -> Result<()> {
        self.pool.flush_all()?;
        self.save_catalog()?;
        // checkpoint: every page is on disk, so the log has nothing left to redo
        self.wal.truncate()
    }

    /// Replays committed WAL records into the buffer pool (they reach the
    /// disk with the next flush) and rebuilds indexes of touched tables.
    /// `touched` accumulates heap file numbers that must have their indexes
    /// rebuilt; it may arrive pre-seeded with repaired index files.
    fn recover_from_wal(&mut self, plan: &wal::RecoveryPlan, touched: &mut HashSet<u32>) -> Result<()> {
        let file_map: std::collections::HashMap<u32, FileId> =
            self.catalog.heap_files().into_iter().collect();
        for (_, _, records) in &plan.committed {
            for rec in records {
                match rec {
                    Record::Insert { file_no, rid, record } => {
                        let file = *file_map
                            .get(file_no)
                            .ok_or_else(|| Error::Runtime(format!("wal references unknown table file {file_no}")))?;
                        while self.pool.page_count(file)? <= rid.page_no {
                            self.pool.alloc_page(file)?;
                        }
                        let occupied = self.pool.read_page(file, rid.page_no, |page| {
                            Ok(page_get(page, rid.slot)?.is_some())
                        })?;
                        if !occupied {
                            self.pool.with_page(file, rid.page_no, |page| {
                                page_put_at(page, rid.slot, record)
                            })?;
                            touched.insert(*file_no);
                        }
                    }
                    Record::DeleteMark { file_no, rid, deleter } => {
                        let file = *file_map
                            .get(file_no)
                            .ok_or_else(|| Error::Runtime(format!("wal references unknown table file {file_no}")))?;
                        if self.pool.page_count(file)? <= rid.page_no {
                            continue;
                        }
                        let unmarked = self.pool.read_page(file, rid.page_no, |page| {
                            match page_get(page, rid.slot)? {
                                Some(rec) if rec.len() >= 8 => {
                                    Ok(u32::from_le_bytes(rec[4..8].try_into().unwrap()) == 0)
                                }
                                _ => Ok(false),
                            }
                        })?;
                        if unmarked {
                            HeapFile::at(file).delete_mark(&mut self.pool, *rid, *deleter)?;
                            touched.insert(*file_no);
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
    fn rebuild_indexes(&mut self, file_no: u32) -> Result<()> {
        let table = self
            .catalog
            .table_metas()
            .into_iter()
            .find(|m| m.file_no == file_no)
            .map(|m| m.name)
            .ok_or_else(|| Error::Runtime(format!("no table owns file {file_no}")))?;
        let ops = self.index_ops(&table)?;
        for (_, ix_file) in &ops {
            self.pool.discard_file(*ix_file);
            self.pool.truncate_file(*ix_file)?;
            BTree::init(&mut self.pool, *ix_file)?;
        }
        for (rid, rec) in self.store_scan_raw(&table)? {
            let (_, _, row) = crate::storage::codec::decode_record(&rec)?;
            for (ci, ix_file) in &ops {
                let key = encode_key(&row[*ci])?;
                BTree::at(*ix_file).insert(&mut self.pool, &key, rid)?;
            }
        }
        Ok(())
    }

    /// Commit bookkeeping shared by explicit COMMIT and autocommit.
    fn commit_trx(&mut self, trx_id: u32, wrote: bool) -> Result<()> {
        self.committed_trxs.insert(trx_id);
        if wrote {
            // log durability first: after this point the transaction commits
            // even if the process dies before its pages are flushed
            self.wal.append(trx_id, &Record::Commit)?;
            self.wal.sync()?;
        }
        self.save_catalog()
    }

    /// Emulates a process crash: dirty buffer-pool pages are lost while
    /// already-appended WAL bytes (OS page cache) survive, like SIGKILL.
    /// Leaks the temp dir of in-memory databases; use file-backed ones.
    pub fn simulate_crash(self) {
        let Database { pool, _temp, .. } = self;
        std::mem::forget(pool);
        std::mem::forget(_temp);
    }

    pub fn execute_sql(&mut self, sql: &str) -> Result<Vec<ResultSet>> {
        let mut session = Session::new();
        self.execute_sql_with(&mut session, sql)
    }

    /// Executes statements within the session's transaction context.
    pub fn execute_sql_with(
        &mut self,
        session: &mut Session,
        sql: &str,
    ) -> Result<Vec<ResultSet>> {
        let stmts = parser::parse(sql)?;
        let mut out = Vec::new();
        for stmt in &stmts {
            match stmt {
                crate::ast::Stmt::Trx(crate::ast::TrxCtl::Begin) => {
                    if session.trx.is_some() {
                        return Err(Error::Runtime("transaction already begun".into()));
                    }
                    let id = self.next_trx_id;
                    self.next_trx_id += 1;
                    session.begin(id, &self.committed_trxs, true);
                }
                crate::ast::Stmt::Trx(crate::ast::TrxCtl::Commit) => {
                    if session.trx.is_none() {
                        return Err(Error::Runtime("no active transaction".into()));
                    }
                    if let Some(trx) = session.trx.take() {
                        let wrote = !trx.undo.is_empty();
                        self.commit_trx(trx.id, wrote)?;
                    }
                }
                crate::ast::Stmt::Trx(crate::ast::TrxCtl::Rollback) => {
                    if session.trx.is_none() {
                        return Err(Error::Runtime("no active transaction".into()));
                    }
                    if let Some(mut trx) = session.trx.take() {
                        self.rollback_trx(&mut trx)?;
                    }
                }
                other => {
                    let autocommit = session.trx.is_none();
                    if autocommit {
                        let id = self.next_trx_id;
                        self.next_trx_id += 1;
                        session.begin(id, &self.committed_trxs, false);
                    }
                    match exec::execute(self, session.trx(), other) {
                        Ok(rs) => {
                            if autocommit {
                                if let Some(trx) = session.trx.take() {
                                    let wrote = !trx.undo.is_empty();
                                    self.commit_trx(trx.id, wrote)?;
                                }
                            }
                            out.push(rs);
                        }
                        Err(e) => {
                            // undo partial statement work; an explicit
                            // transaction stays open for retry or rollback
                            if let Some(mut trx) = session.trx.take() {
                                self.rollback_trx(&mut trx)?;
                                if !autocommit {
                                    session.trx = Some(trx);
                                }
                            }
                            return Err(e);
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// Rolls back any open transaction when a session goes away.
    pub fn rollback_session(&mut self, session: &mut Session) -> Result<()> {
        if let Some(mut trx) = session.trx.take() {
            self.rollback_trx(&mut trx)?;
        }
        Ok(())
    }

    fn rollback_trx(&mut self, trx: &mut TrxState) -> Result<()> {
        while let Some(undo) = trx.undo.pop() {
            match undo {
                Undo::Insert { table, rid, row } => {
                    let file = self.catalog.table(&table)?.heap.file;
                    HeapFile::at(file).delete(&mut self.pool, rid)?;
                    for (ci, ix_file) in self.index_ops(&table)? {
                        let key = encode_key(&row[ci])?;
                        BTree::at(ix_file).delete(&mut self.pool, &key, rid)?;
                    }
                }
                Undo::DeleteMark { table, rid } => {
                    let file = self.catalog.table(&table)?.heap.file;
                    HeapFile::at(file).delete_mark(&mut self.pool, rid, 0)?;
                }
                Undo::Update { table, old_rid, new_rid, new_row } => {
                    let file = self.catalog.table(&table)?.heap.file;
                    HeapFile::at(file).delete(&mut self.pool, new_rid)?;
                    for (ci, ix_file) in self.index_ops(&table)? {
                        let key = encode_key(&new_row[ci])?;
                        BTree::at(ix_file).delete(&mut self.pool, &key, new_rid)?;
                    }
                    HeapFile::at(file).delete_mark(&mut self.pool, old_rid, 0)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub(crate) fn catalog_mut(&mut self) -> &mut Catalog {
        &mut self.catalog
    }

    pub(crate) fn new_table_heap(&mut self, _name: &str) -> Result<HeapStore> {
        let file_no = self.next_table_file;
        self.next_table_file += 1;
        let path = self.data_dir.join("tables").join(format!("{file_no:06}.dbf"));
        let file = self.pool.create_file(&path)?;
        HeapFile::init(&mut self.pool, file)?;
        Ok(HeapStore { file, file_no })
    }

    pub(crate) fn new_index_heap(&mut self, _name: &str) -> Result<IndexStore> {
        let file_no = self.next_index_file;
        self.next_index_file += 1;
        let path = self.data_dir.join("indexes").join(format!("{file_no:06}.idxf"));
        let file = self.pool.create_file(&path)?;
        BTree::init(&mut self.pool, file)?;
        Ok(IndexStore { file, file_no })
    }

    pub(crate) fn save_catalog(&self) -> Result<()> {
        let snap = CatalogSnapshot {
            next_table_file: self.next_table_file,
            next_index_file: self.next_index_file,
            next_trx_id: self.next_trx_id,
            committed_trxs: self.committed_trxs.iter().copied().collect(),
            tables: self.catalog.table_metas(),
            indexes: self.catalog.index_metas(),
        };
        let bytes = encode_catalog(&snap);
        std::fs::write(self.data_dir.join("catalog.bin"), bytes)
            .map_err(|e| Error::Runtime(format!("cannot write catalog: {e}")))
    }

    /// (column index, index file) pairs for every index on `table`.
    pub(crate) fn index_ops(&self, table: &str) -> Result<Vec<(usize, FileId)>> {
        let schema = &self.catalog.table(table)?.schema;
        Ok(self
            .catalog
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
    pub(crate) fn store_scan_raw(&mut self, name: &str) -> Result<Vec<(Rid, Vec<u8>)>> {
        let file = self.catalog.table(name)?.heap.file;
        let heap = HeapFile::at(file);
        let mut out = Vec::new();
        heap.for_each(&mut self.pool, |rid, rec| {
            out.push((rid, rec.to_vec()));
            Ok(())
        })?;
        Ok(out)
    }

    pub(crate) fn store_get_records(
        &mut self,
        name: &str,
        rids: &[Rid],
    ) -> Result<Vec<(Rid, Vec<u8>)>> {
        let file = self.catalog.table(name)?.heap.file;
        let heap = HeapFile::at(file);
        let mut out = Vec::new();
        for rid in rids {
            let rec = heap.get(&mut self.pool, *rid)?;
            out.push((*rid, rec));
        }
        Ok(out)
    }

    pub(crate) fn store_insert(
        &mut self,
        name: &str,
        row: Vec<Value>,
        creator: u32,
    ) -> Result<Rid> {
        let (file, file_no) = {
            let t = self.catalog.table(name)?;
            (t.heap.file, t.heap.file_no)
        };
        let heap = HeapFile::at(file);
        let data = encode_record(creator, 0, &row);
        let rid = heap.insert(&mut self.pool, &data)?;
        self.wal.append(creator, &Record::Insert { file_no, rid, record: data.clone() })?;
        for (ci, ix_file) in self.index_ops(name)? {
            let key = encode_key(&row[ci])?;
            BTree::at(ix_file).insert(&mut self.pool, &key, rid)?;
        }
        Ok(rid)
    }

    /// MVCC delete: mark records with the deleter's trx id (index untouched,
    /// stale entries are filtered by visibility on read).
    pub(crate) fn store_delete_mark(
        &mut self,
        name: &str,
        rids: &[Rid],
        deleter: u32,
    ) -> Result<()> {
        let (file, file_no) = {
            let t = self.catalog.table(name)?;
            (t.heap.file, t.heap.file_no)
        };
        let heap = HeapFile::at(file);
        for rid in rids {
            heap.delete_mark(&mut self.pool, *rid, deleter)?;
            self.wal.append(deleter, &Record::DeleteMark { file_no, rid: *rid, deleter })?;
        }
        Ok(())
    }

    /// MVCC update: delete-mark the old version, insert a new one. Index
    /// entries for the new version are added; old entries stay so older
    /// snapshots can still find them (filtered by visibility on read).
    pub(crate) fn store_update_versions(
        &mut self,
        name: &str,
        updates: &[(Rid, Vec<Value>)],
        trx_id: u32,
    ) -> Result<Vec<Rid>> {
        let (file, file_no) = {
            let t = self.catalog.table(name)?;
            (t.heap.file, t.heap.file_no)
        };
        let heap = HeapFile::at(file);
        let ops = self.index_ops(name)?;
        let mut new_rids = Vec::with_capacity(updates.len());
        for (rid, new_row) in updates {
            heap.delete_mark(&mut self.pool, *rid, trx_id)?;
            self.wal
                .append(trx_id, &Record::DeleteMark { file_no, rid: *rid, deleter: trx_id })?;
            let data = encode_record(trx_id, 0, new_row);
            let new_rid = heap.insert(&mut self.pool, &data)?;
            self.wal.append(
                trx_id,
                &Record::Insert { file_no, rid: new_rid, record: data.clone() },
            )?;
            for (ci, ix_file) in &ops {
                let key = encode_key(&new_row[*ci])?;
                BTree::at(*ix_file).insert(&mut self.pool, &key, new_rid)?;
            }
            new_rids.push(new_rid);
        }
        Ok(new_rids)
    }
}

fn dir_err(dir: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::Runtime(format!("cannot create dir {}: {e}", dir.display()))
}
