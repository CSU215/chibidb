pub mod ast;
pub mod catalog;
pub mod client;
pub mod config;
pub mod datetime;
mod error;
pub mod exec;
pub mod index;
pub mod instance;
pub mod lexer;
pub mod parser;
pub mod pipeline;
pub mod protocol;
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
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::catalog::meta::{decode_catalog, encode_catalog, CatalogSnapshot};
use crate::catalog::{Catalog, ColumnDesc, HeapStore, IndexStore, Schema};
use crate::config::Config;
use crate::index::{encode_key, BTree};
use crate::pipeline::{ExecuteStage, OptimizeStage, Pipeline, ResolveStage, SqlEvent};
use crate::storage::codec::{decode_record, encode_record};
use crate::storage::engine::{HeapEngine, TableEngine};
use crate::storage::slotted::{page_get, page_put_at};
use crate::storage::{BufferPool, DiskManager, FileId, HeapFile, Rid};
use crate::trx::{TrxState, Undo};
use crate::value::Value;
use crate::wal::{Record, Wal};

pub use crate::trx::Session;

pub struct Database {
    config: Config,
    catalog: RwLock<Catalog>,
    pool: BufferPool,
    wal: Wal,
    data_dir: PathBuf,
    next_table_file: AtomicU32,
    next_index_file: AtomicU32,
    next_trx_id: AtomicU32,
    committed_trxs: RwLock<HashSet<u32>>,
    /// Transactions that have begun but not committed/rolled back yet. The
    /// log must not be truncated while this is non-empty, or a later COMMIT
    /// would lose its redo records.
    open_trxs: RwLock<HashSet<u32>>,
    wal_checkpoint_threshold: AtomicU64,
    _temp: Option<tempfile::TempDir>,
}

impl Database {
    /// A throwaway database in an automatically-cleaned temporary directory.
    pub fn open_in_memory() -> Result<Self> {
        Self::open_in_memory_with_config(&Config::default())
    }

    /// Like [`Database::open_in_memory`] but applying `config` defaults.
    pub fn open_in_memory_with_config(config: &Config) -> Result<Self> {
        let temp = tempfile::tempdir()
            .map_err(|e| Error::Runtime(format!("cannot create temp dir: {e}")))?;
        let db = Self::open_with_config(temp.path(), config)?;
        Ok(Self { _temp: Some(temp), ..db })
    }

    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_config(path, &Config::default())
    }

    /// Opens (or creates) a database directory using `config` defaults for
    /// newly created files. Existing files describe themselves and take
    /// precedence over these defaults.
    pub fn open_with_config(path: &Path, config: &Config) -> Result<Self> {
        if config.storage.page_size as usize != crate::storage::PAGE_SIZE {
            return Err(Error::Runtime(format!(
                "storage.page_size {} is not supported yet (this build uses {})",
                config.storage.page_size,
                crate::storage::PAGE_SIZE
            )));
        }
        let tables_dir = path.join("tables");
        let indexes_dir = path.join("indexes");
        std::fs::create_dir_all(&tables_dir).map_err(dir_err(&tables_dir))?;
        std::fs::create_dir_all(&indexes_dir).map_err(dir_err(&indexes_dir))?;
        let mut disk = DiskManager::new();
        if config.storage.double_write {
            let dwb_path = path.join("dwb.bin");
            crate::storage::dwb::recover(&dwb_path, crate::storage::dwb::write_page_at)?;
            disk.enable_double_write(&dwb_path)?;
        }
        let pool = BufferPool::new(disk, config.storage.buffer_pool_frames);
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
                HeapFile::open_or_repair(&pool, file)?;
                let schema = Schema {
                    columns: meta
                        .columns
                        .iter()
                        .map(|c| ColumnDesc {
                            owner: None,
                            name: c.name.clone(),
                            dtype: c.dtype,
                            not_null: c.not_null,
                            primary_key: c.primary_key,
                            unique: c.unique,
                            default: c.default.clone(),
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
                if BTree::open_or_repair(&pool, file)? {
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
                    ix.unique,
                    IndexStore { file, file_no: ix.file_no },
                )?;
            }
            for v in &snap.views {
                catalog.create_view(&v.name, v.sql.clone())?;
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

        let db = Self {
            config: config.clone(),
            catalog: RwLock::new(catalog),
            pool,
            wal: Wal::open(&wal_path)?,
            data_dir: path.to_path_buf(),
            next_table_file: AtomicU32::new(next_table_file),
            next_index_file: AtomicU32::new(next_index_file),
            next_trx_id: AtomicU32::new(next_trx_id),
            committed_trxs: RwLock::new(committed_trxs),
            open_trxs: RwLock::new(HashSet::new()),
            wal_checkpoint_threshold: AtomicU64::new(config.wal.checkpoint_threshold),
            _temp: None,
        };
        db.recover_from_wal(&plan, &mut touched)?;

        // never hand a crashed transaction's id to a new transaction
        if plan.max_trx_id >= db.next_trx_id.load(Ordering::SeqCst) {
            db.next_trx_id.store(plan.max_trx_id.saturating_add(1), Ordering::SeqCst);
        }
        let committed_repaired = {
            let committed = db.committed_trxs.read();
            plan.committed_ids.iter().any(|id| !committed.contains(id))
        };
        if committed_repaired {
            db.committed_trxs.write().extend(plan.committed_ids);
        }
        // persist the repaired committed set so it survives a second crash
        // even if nothing else is written in this session
        if committed_repaired {
            db.save_catalog()?;
        }
        Ok(db)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn flush(&self) -> Result<()> {
        if !self.open_trxs.read().is_empty() {
            // truncating the log now would drop the open transaction's redo
            // records, so its later COMMIT could not be recovered
            return Err(Error::Runtime(
                "cannot flush while transactions are open".into(),
            ));
        }
        self.flush_inner()
    }

    pub(crate) fn flush_inner(&self) -> Result<()> {
        self.pool.flush_all()?;
        self.save_catalog()?;
        // checkpoint: every page is on disk, so the log has nothing left to redo
        self.wal.truncate()
    }

    /// Physically removes rows no transaction can ever see again:
    /// delete-marked rows whose deleter committed, and orphan versions whose
    /// creator never committed (left behind by a crashed transaction).
    /// Stale index entries of purged rows are removed too. Must run with no
    /// open transactions (the VACUUM statement enforces this).
    pub(crate) fn vacuum(&self) -> Result<usize> {
        let mut purged = 0;
        let committed = self.committed_trxs.read().clone();
        let metas = self.catalog().table_metas();
        for meta in metas {
            let ops = self.index_ops(&meta.name)?;
            for (rid, rec) in self.store_scan_raw(&meta.name)? {
                let (creator, deleter, row) = crate::storage::codec::decode_record(&rec)?;
                let dead = !committed.contains(&creator)
                    || (deleter != 0 && committed.contains(&deleter));
                if !dead {
                    continue;
                }
                for (ci, ix_file) in &ops {
                    let key = encode_key(&row[*ci])?;
                    BTree::at(*ix_file).delete(&self.pool, &key, rid)?;
                }
                let file = self.catalog().table(&meta.name)?.heap.file;
                HeapFile::at(file).delete(&self.pool, rid)?;
                purged += 1;
            }
        }
        Ok(purged)
    }

    /// Replays committed WAL records into the buffer pool (they reach the
    /// disk with the next flush) and rebuilds indexes of touched tables.
    /// `touched` accumulates heap file numbers that must have their indexes
    /// rebuilt; it may arrive pre-seeded with repaired index files.
    fn recover_from_wal(&self, plan: &wal::RecoveryPlan, touched: &mut HashSet<u32>) -> Result<()> {
        let file_map: std::collections::HashMap<u32, FileId> =
            self.catalog().heap_files().into_iter().collect();
        for (_, _, records) in &plan.committed {
            for rec in records {
                match rec {
                    Record::Insert { file_no, rid, record } => {
                        // records of dropped tables (file no longer in the
                        // catalog) are stale and skipped
                        let Some(file) = file_map.get(file_no) else { continue };
                        let file = *file;
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
                        let Some(file) = file_map.get(file_no) else { continue };
                        let file = *file;
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
                            HeapFile::at(file).delete_mark(&self.pool, *rid, *deleter)?;
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
            let (_, _, row) = crate::storage::codec::decode_record(&rec)?;
            for (ci, ix_file) in &ops {
                let key = encode_key(&row[*ci])?;
                BTree::at(*ix_file).insert(&self.pool, &key, rid)?;
            }
        }
        Ok(())
    }

    /// Commit bookkeeping shared by explicit COMMIT and autocommit.
    fn commit_trx(&self, trx_id: u32, wrote: bool) -> Result<()> {
        self.committed_trxs.write().insert(trx_id);
        self.open_trxs.write().remove(&trx_id);
        if !wrote {
            // A read-only transaction created no versioned rows, so no future
            // snapshot needs its id and its bookkeeping need not hit disk.
            // Skipping the catalog rewrite keeps SELECT cheap.
            return Ok(());
        }
        // log durability first: after this point the transaction commits
        // even if the process dies before its pages are flushed
        self.wal.append(trx_id, &Record::Commit)?;
        self.wal.sync()?;
        self.save_catalog()?;
        // opportunistic checkpoint once the log outgrew its budget and no
        // open transaction is counting on its contents
        if self.wal.len()? > self.wal_checkpoint_threshold.load(Ordering::Relaxed)
            && self.open_trxs.read().is_empty()
        {
            self.flush()?;
        }
        Ok(())
    }

    /// Emulates a process crash: dirty buffer-pool pages are lost while
    /// already-appended WAL bytes (OS page cache) survive, like SIGKILL.
    /// Leaks the temp dir of in-memory databases; use file-backed ones.
    pub fn simulate_crash(self) {
        let Database { pool, _temp, .. } = self;
        std::mem::forget(pool);
        std::mem::forget(_temp);
    }

    pub fn execute_sql(&self, sql: &str) -> Result<Vec<ResultSet>> {
        let mut session = Session::new();
        self.execute_sql_with(&mut session, sql)
    }

    /// Runs a physical operator tree to completion under an auto-committed
    /// read transaction, returning the produced rows. Used by tests and the
    /// future operator-based executor.
    pub fn collect_plan(
        &self,
        session: &mut Session,
        plan: &mut dyn crate::exec::operator::PhysicalOperator,
    ) -> Result<Vec<Vec<Value>>> {
        let autocommit = session.trx.is_none();
        let read_only = autocommit
            && plan.output_kind() == crate::exec::operator::OutputKind::Rows;
        if autocommit {
            if read_only {
                let committed = self.committed_snapshot();
                session.begin_readonly(&committed);
            } else {
                let id = self.alloc_trx_id();
                let committed = self.committed_snapshot();
                session.begin(id, &committed, false);
                self.open_trxs.write().insert(id);
            }
        }
        let mut out = Vec::new();
        let result = run_plan(self, session, plan, &mut out);
        if autocommit && let Some(mut trx) = session.trx.take() {
            if result.is_ok() {
                if !read_only {
                    self.commit_trx(trx.id, false)?;
                }
            } else {
                self.rollback_trx(&mut trx)?;
                if !read_only {
                    self.open_trxs.write().remove(&trx.id);
                }
            }
        }
        result.map(|()| out)
    }

    /// Executes statements within the session's transaction context.
    pub fn execute_sql_with(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<Vec<ResultSet>> {
        let stmts = parser::parse(sql)?;
        let mut out = Vec::new();
        for stmt in &stmts {
            if let Some(rs) = self.execute_stmt_with(session, stmt)? {
                out.push(rs);
            }
        }
        Ok(out)
    }

    /// Executes one parsed statement. Transaction-control statements produce
    /// no result set (`None`).
    pub(crate) fn execute_stmt_with(
        &self,
        session: &mut Session,
        stmt: &crate::ast::Stmt,
    ) -> Result<Option<ResultSet>> {
        let pipeline = Pipeline::new(vec![
            Box::new(ResolveStage),
            Box::new(OptimizeStage),
            Box::new(ExecuteStage),
        ]);
        match stmt {
            crate::ast::Stmt::Trx(crate::ast::TrxCtl::Begin) => {
                if session.trx.is_some() {
                    return Err(Error::Runtime("transaction already begun".into()));
                }
                let id = self.alloc_trx_id();
                let committed = self.committed_snapshot();
                session.begin(id, &committed, true);
                self.open_trxs.write().insert(id);
                Ok(None)
            }
            crate::ast::Stmt::Trx(crate::ast::TrxCtl::Commit) => {
                if session.trx.is_none() {
                    return Err(Error::Runtime("no active transaction".into()));
                }
                if let Some(trx) = session.trx.take() {
                    let wrote = !trx.undo.is_empty();
                    self.commit_trx(trx.id, wrote)?;
                }
                Ok(None)
            }
            crate::ast::Stmt::Trx(crate::ast::TrxCtl::Rollback) => {
                if session.trx.is_none() {
                    return Err(Error::Runtime("no active transaction".into()));
                }
                if let Some(mut trx) = session.trx.take() {
                    self.rollback_trx(&mut trx)?;
                    self.open_trxs.write().remove(&trx.id);
                }
                Ok(None)
            }
            other => {
                let read_only = is_read_only(other);
                let autocommit = session.trx.is_none();
                if autocommit {
                    if read_only {
                        let committed = self.committed_snapshot();
                        session.begin_readonly(&committed);
                    } else {
                        let id = self.alloc_trx_id();
                        let committed = self.committed_snapshot();
                        session.begin(id, &committed, false);
                        self.open_trxs.write().insert(id);
                    }
                }
                let mut event = SqlEvent::new(other);
                match pipeline.run(self, session, &mut event) {
                    Ok(()) => {
                        let rs = event.result.take().expect("execute stage produced no result");
                        if autocommit
                            && let Some(trx) = session.trx.take()
                            && !read_only
                        {
                            let wrote = !trx.undo.is_empty();
                            self.commit_trx(trx.id, wrote)?;
                        }
                        Ok(Some(rs))
                    }
                    Err(e) => {
                        // undo partial statement work; an explicit
                        // transaction stays open for retry or rollback
                        if let Some(mut trx) = session.trx.take()
                            && !read_only
                        {
                            self.rollback_trx(&mut trx)?;
                            if !autocommit {
                                session.trx = Some(trx);
                            } else {
                                self.open_trxs.write().remove(&trx.id);
                            }
                        }
                        Err(e)
                    }
                }
            }
        }
    }

    /// Rolls back any open transaction when a session goes away.
    pub fn rollback_session(&self, session: &mut Session) -> Result<()> {
        if let Some(mut trx) = session.trx.take() {
            self.rollback_trx(&mut trx)?;
            self.open_trxs.write().remove(&trx.id);
        }
        Ok(())
    }

    /// Whether any session other than `trx_id` has a transaction open.
    pub(crate) fn has_open_trxs_excluding(&self, trx_id: u32) -> bool {
        self.open_trxs.read().iter().any(|&id| id != trx_id)
    }

    /// Overrides the auto-checkpoint log budget in bytes; mainly for tests.
    pub fn set_wal_checkpoint_threshold(&self, bytes: u64) {
        self.wal_checkpoint_threshold.store(bytes, Ordering::Relaxed);
    }

    /// Reserves the next transaction id.
    fn alloc_trx_id(&self) -> u32 {
        self.next_trx_id.fetch_add(1, Ordering::SeqCst)
    }

    /// A copy of the committed set, the snapshot a new transaction sees.
    fn committed_snapshot(&self) -> HashSet<u32> {
        self.committed_trxs.read().clone()
    }

    fn rollback_trx(&self, trx: &mut TrxState) -> Result<()> {
        while let Some(undo) = trx.undo.pop() {
            match undo {
                Undo::Insert { table, rid, row } => {
                    let file = self.catalog().table(&table)?.heap.file;
                    HeapFile::at(file).delete(&self.pool, rid)?;
                    for (ci, ix_file) in self.index_ops(&table)? {
                        let key = encode_key(&row[ci])?;
                        BTree::at(ix_file).delete(&self.pool, &key, rid)?;
                    }
                }
                Undo::DeleteMark { table, rid } => {
                    let file = self.catalog().table(&table)?.heap.file;
                    HeapFile::at(file).delete_mark(&self.pool, rid, 0)?;
                }
                Undo::Update { table, old_rid, new_rid, new_row } => {
                    let file = self.catalog().table(&table)?.heap.file;
                    HeapFile::at(file).delete(&self.pool, new_rid)?;
                    for (ci, ix_file) in self.index_ops(&table)? {
                        let key = encode_key(&new_row[ci])?;
                        BTree::at(ix_file).delete(&self.pool, &key, new_rid)?;
                    }
                    HeapFile::at(file).delete_mark(&self.pool, old_rid, 0)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn catalog(&self) -> RwLockReadGuard<'_, Catalog> {
        self.catalog.read()
    }

    /// Whether a table with `name` exists in this database.
    pub(crate) fn table_exists(&self, name: &str) -> bool {
        self.catalog().table(name).is_ok()
    }

    pub(crate) fn catalog_mut(&self) -> RwLockWriteGuard<'_, Catalog> {
        self.catalog.write()
    }

    pub(crate) fn new_table_heap(&self, _name: &str) -> Result<HeapStore> {
        let file_no = self.next_table_file.fetch_add(1, Ordering::SeqCst);
        let path = self.data_dir.join("tables").join(format!("{file_no:06}.dbf"));
        let file = self.pool.create_file(&path)?;
        HeapFile::init(&self.pool, file)?;
        Ok(HeapStore { file, file_no })
    }

    pub(crate) fn new_index_heap(&self, _name: &str) -> Result<IndexStore> {
        let file_no = self.next_index_file.fetch_add(1, Ordering::SeqCst);
        let path = self.data_dir.join("indexes").join(format!("{file_no:06}.idxf"));
        let file = self.pool.create_file(&path)?;
        BTree::init(&self.pool, file)?;
        Ok(IndexStore { file, file_no })
    }

    pub(crate) fn save_catalog(&self) -> Result<()> {
        let snap = CatalogSnapshot {
            next_table_file: self.next_table_file.load(Ordering::SeqCst),
            next_index_file: self.next_index_file.load(Ordering::SeqCst),
            next_trx_id: self.next_trx_id.load(Ordering::SeqCst),
            committed_trxs: self.committed_trxs.read().iter().copied().collect(),
            tables: self.catalog().table_metas(),
            indexes: self.catalog().index_metas(),
            views: self.catalog().view_metas(),
        };
        let bytes = encode_catalog(&snap);
        std::fs::write(self.data_dir.join("catalog.bin"), bytes)
            .map_err(|e| Error::Runtime(format!("cannot write catalog: {e}")))
    }

    /// Drops a table: catalog first (durability), then its heap and index
    /// files. A crash in between leaves harmless orphan files behind.
    pub(crate) fn drop_table(&self, name: &str) -> Result<()> {
        let dropped = self.catalog_mut().drop_table(name)?;
        self.save_catalog()?;
        for file in std::iter::once(dropped.heap_file).chain(dropped.index_files) {
            let path = self.pool.close_file(file)?;
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                // another DROP in the same statement batch may already have it
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(Error::Runtime(format!(
                        "cannot delete file {}: {e}",
                        path.display()
                    )))
                }
            }
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
        let file = self.catalog().table(name)?.heap.file;
        let engine = HeapEngine::new(file);
        let mut scanner = engine.scan(&self.pool)?;
        let mut out = Vec::new();
        while let Some(row) = scanner.next(&self.pool)? {
            out.push(row);
        }
        Ok(out)
    }

    /// Enforces UNIQUE / PRIMARY KEY constraints for `row` using the
    /// constraint-backed indexes and MVCC visibility. `exclude` skips the
    /// row being updated; `claimed` catches duplicates among rows touched by
    /// the same statement before they reach the index. Claims are keyed by
    /// column index so equal values in different constraint columns do not
    /// collide.
    pub(crate) fn check_unique(
        &self,
        table: &str,
        row: &[Value],
        exclude: Option<Rid>,
        trx: &TrxState,
        claimed: &mut Vec<(usize, Vec<u8>)>,
    ) -> Result<()> {
        let (checks, heap_file) = {
            let catalog = self.catalog();
            let schema = &catalog.table(table)?.schema;
            let checks: Vec<(usize, FileId, String)> = catalog
                .unique_indexes_for(table)
                .into_iter()
                .map(|ix| {
                    let ci = schema.index_of(&ix.column).expect("index column validated");
                    (ci, ix.store.file, ix.column.clone())
                })
                .collect();
            (checks, catalog.table(table)?.heap.file)
        };
        if checks.is_empty() {
            return Ok(());
        }
        let heap = HeapFile::at(heap_file);
        for (ci, ix_file, column) in checks {
            if matches!(row[ci], Value::Null) {
                continue; // UNIQUE permits multiple NULLs
            }
            let key = encode_key(&row[ci])?;
            if claimed.iter().any(|(c, k)| *c == ci && k == &key) {
                return Err(Error::Runtime(format!("duplicate key: {table}({column})")));
            }
            claimed.push((ci, key.clone()));
            for rid in BTree::at(ix_file).search(&self.pool, &key)? {
                if Some(rid) == exclude {
                    continue;
                }
                let rec = heap.get(&self.pool, rid)?;
                let (creator, deleter, _) = decode_record(&rec)?;
                if trx.visible(creator, deleter) {
                    return Err(Error::Runtime(format!("duplicate key: {table}({column})")));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn store_insert(
        &self,
        name: &str,
        row: Vec<Value>,
        creator: u32,
    ) -> Result<Rid> {
        let (file, file_no) = {
            let catalog = self.catalog();
            let t = catalog.table(name)?;
            (t.heap.file, t.heap.file_no)
        };
        let heap = HeapFile::at(file);
        let data = encode_record(creator, 0, &row);
        let rid = heap.insert(&self.pool, &data)?;
        self.wal.append(creator, &Record::Insert { file_no, rid, record: data.clone() })?;
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
        deleter: u32,
    ) -> Result<()> {
        let (file, file_no) = {
            let catalog = self.catalog();
            let t = catalog.table(name)?;
            (t.heap.file, t.heap.file_no)
        };
        let heap = HeapFile::at(file);
        for rid in rids {
            heap.delete_mark(&self.pool, *rid, deleter)?;
            self.wal.append(deleter, &Record::DeleteMark { file_no, rid: *rid, deleter })?;
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
        trx_id: u32,
    ) -> Result<Vec<Rid>> {
        let (file, file_no) = {
            let catalog = self.catalog();
            let t = catalog.table(name)?;
            (t.heap.file, t.heap.file_no)
        };
        let heap = HeapFile::at(file);
        let ops = self.index_ops(name)?;
        let mut new_rids = Vec::with_capacity(updates.len());
        for (rid, new_row) in updates {
            heap.delete_mark(&self.pool, *rid, trx_id)?;
            self.wal
                .append(trx_id, &Record::DeleteMark { file_no, rid: *rid, deleter: trx_id })?;
            let data = encode_record(trx_id, 0, new_row);
            let new_rid = heap.insert(&self.pool, &data)?;
            self.wal.append(
                trx_id,
                &Record::Insert { file_no, rid: new_rid, record: data.clone() },
            )?;
            for (ci, ix_file) in &ops {
                let key = encode_key(&new_row[*ci])?;
                BTree::at(*ix_file).insert(&self.pool, &key, new_rid)?;
            }
            new_rids.push(new_rid);
        }
        Ok(new_rids)
    }
}

/// Whether a statement only reads, so its autocommit needs no transaction
/// bookkeeping. Everything else (DML, DDL, CHECKPOINT, VACUUM) may mutate
/// state and takes the normal round trip.
pub(crate) fn is_read_only(stmt: &crate::ast::Stmt) -> bool {
    matches!(
        stmt,
        crate::ast::Stmt::Select(_) | crate::ast::Stmt::Explain(_)
    )
}

/// Drives one operator tree to completion, appending rows to `out`.
fn run_plan(
    db: &Database,
    session: &mut Session,
    plan: &mut dyn crate::exec::operator::PhysicalOperator,
    out: &mut Vec<Vec<Value>>,
) -> Result<()> {
    let mut ctx = crate::exec::operator::ExecContext { db, trx: session.trx(), outer: None };
    plan.open(&mut ctx)?;
    while let Some(row) = plan.next(&mut ctx)? {
        out.push(row);
    }
    plan.close()?;
    Ok(())
}

fn dir_err(dir: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::Runtime(format!("cannot create dir {}: {e}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_autocommit_does_not_touch_bookkeeping() {
        let db = Database::open_in_memory().unwrap();
        let before = db.next_trx_id.load(Ordering::SeqCst);

        db.execute_sql("select 1;").unwrap();

        assert_eq!(
            db.next_trx_id.load(Ordering::SeqCst),
            before,
            "read-only must not allocate a trx id"
        );
        assert!(
            db.committed_trxs.read().is_empty(),
            "read-only must not join committed"
        );
        assert!(db.open_trxs.read().is_empty(), "read-only must not stay open");
    }

    #[test]
    fn write_autocommit_registers_its_transaction() {
        let db = Database::open_in_memory().unwrap();
        db.execute_sql("create table t (id int);").unwrap();
        let before = db.next_trx_id.load(Ordering::SeqCst);

        db.execute_sql("insert into t values (1);").unwrap();

        assert!(
            db.next_trx_id.load(Ordering::SeqCst) > before,
            "write must allocate a trx id"
        );
        assert!(
            db.committed_trxs.read().contains(&before),
            "write must be committed"
        );
        assert!(db.open_trxs.read().is_empty(), "write must not stay open");
    }
}
