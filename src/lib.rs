pub mod catalog;
pub mod config;
pub mod db;
mod error;
pub mod exec;
pub mod index;
pub mod net;
pub mod sql;
pub mod storage;
pub mod wal;

// Compatibility re-exports: the implementation now lives in subdirectories,
// but these paths stay valid for callers (and the crate's own `crate::x` paths).
pub use db::{instance, transaction, trx};
pub use net::{client, http, mysql, protocol, render, server, wire};
pub use sql::{ast, datetime, lexer, parser, pipeline, result, value};

pub use error::{Error, Result};
pub use net::run_repl;
pub use sql::result::ResultSet;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::{Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// A per-database 2PL write lock. It is reference-counted so callers can hold
/// it (and wait on it) without holding the database lock itself, which keeps
/// lock acquisition from deadlocking against statement execution.
pub(crate) struct DatabaseWriteLock {
    owner: Mutex<Option<u64>>,
    cv: Condvar,
    timeout: Duration,
}

impl DatabaseWriteLock {
    fn new(timeout_ms: u64) -> Self {
        Self {
            owner: Mutex::new(None),
            cv: Condvar::new(),
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    /// Takes the lock for `session_id`, blocking until it is free or the
    /// configured timeout elapses. Re-entrant by owner id.
    pub(crate) fn acquire(&self, session_id: u64) -> Result<()> {
        let mut owner = self.owner.lock();
        if *owner == Some(session_id) {
            return Ok(());
        }
        while owner.is_some() {
            let timed_out = self.cv.wait_for(&mut owner, self.timeout).timed_out();
            if timed_out && owner.is_some() {
                return Err(Error::Runtime("lock wait timeout".into()));
            }
        }
        *owner = Some(session_id);
        Ok(())
    }

    pub(crate) fn release(&self, session_id: u64) {
        let mut owner = self.owner.lock();
        if *owner == Some(session_id) {
            *owner = None;
            self.cv.notify_one();
        }
    }
}

use crate::catalog::meta::{decode_catalog, encode_catalog, CatalogSnapshot};
use crate::catalog::{Catalog, ColumnDesc, HeapStore, IndexStore, Schema};
use crate::config::{Config, ConflictStrategy, EngineKind, ExecutionMode, Isolation, PageLayout};
use crate::index::{encode_key, BTree};
use crate::pipeline::{ExecuteStage, OptimizeStage, Pipeline, ResolveStage, SqlEvent};
use crate::storage::codec::{decode_record, encode_record};
use crate::storage::engine::{HeapEngine, TableStorage};
use crate::storage::lsm::engine::{LsmEngine, LSM_FILE_ID};
use crate::storage::{BufferPool, DiskManager, FileId, HeapFile, LobStore, Rid};
use crate::db::lockmgr::LockManager;
use crate::transaction::TransactionManager;
use crate::trx::{TrxState, Undo};
use crate::value::Value;
use crate::wal::{Record, Wal};

pub use crate::trx::Session;

/// SSTable block size for LSM-backed tables.
const LSM_BLOCK_SIZE: usize = 4096;

/// WAL-replay routing: file number to its engine kind and handle.
type StorageMap = std::collections::HashMap<u32, (EngineKind, Arc<dyn TableStorage>)>;

pub struct Database {
    config: Config,
    catalog: RwLock<Catalog>,
    pool: BufferPool,
    /// Out-of-line storage for long string values.
    lobs: LobStore,
    wal: Wal,
    data_dir: PathBuf,
    next_table_file: AtomicU32,
    next_index_file: AtomicU32,
    /// Transaction id source, committed set and open set.
    trx: TransactionManager,
    wal_checkpoint_threshold: AtomicU64,
    conflict: ConflictStrategy,
    /// The per-database 2PL write lock, shared with the instance layer so it
    /// can be acquired before the database lock.
    writer: Arc<DatabaseWriteLock>,
    /// Row-level tuple locks, held by a transaction until it ends. Writers of
    /// different rows proceed concurrently; same-row writers queue.
    locks: LockManager,
    /// Serializes a whole checkpoint (`flush_inner`): the buffer-pool flush,
    /// each engine's flush, the catalog save and the WAL truncation. Per-
    /// statement checkpoints already hold the database write lock, so this is
    /// for callers that do not: concurrent `Instance::flush` threads, or a
    /// direct `Database::flush`. The pool guards its own double-write
    /// protocol; the later steps need the same exclusion to stay one unit.
    checkpoint_lock: Mutex<()>,
    /// Serializes catalog saves: a commit and a checkpoint can run at the same
    /// time and would otherwise race on the shared `catalog.tmp` staging path.
    catalog_lock: Mutex<()>,
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
        let tables_dir = path.join("tables");
        let indexes_dir = path.join("indexes");
        std::fs::create_dir_all(&tables_dir).map_err(dir_err(&tables_dir))?;
        std::fs::create_dir_all(&indexes_dir).map_err(dir_err(&indexes_dir))?;
        let disk = DiskManager::new();
        if config.storage.double_write {
            let dwb_path = path.join("dwb.bin");
            crate::storage::dwb::recover(&dwb_path, crate::storage::dwb::write_page_at)?;
            disk.enable_double_write(&dwb_path)?;
        }
        let pool = BufferPool::new_with_eviction(
            disk,
            config.storage.buffer_pool_frames,
            config.storage.eviction,
        );
        let mut catalog = Catalog::default();
        let mut next_table_file = 0;
        let mut next_index_file = 0;
        let mut next_trx_id = 1;
        let mut clog_base = 0;
        let mut committed_trxs: HashSet<u64> = HashSet::new();
        // tables whose index file headers were rebuilt after a crash; their
        // contents must be re-derived from the heap
        let mut repaired_index_tables: Vec<String> = Vec::new();

        let catalog_path = path.join("catalog.bin");
        if catalog_path.exists() {
            let bytes = std::fs::read(&catalog_path)
                .map_err(|e| Error::Runtime(format!("cannot read catalog: {e}")))?;
            let snap = decode_catalog(&bytes)?;
            for meta in &snap.tables {
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
                let (heap, engine): (HeapStore, Arc<dyn TableStorage>) = match meta.engine {
                    EngineKind::Heap => {
                        let fpath = tables_dir.join(format!("{:06}.dbf", meta.file_no));
                        let file = pool.open_file(&fpath)?;
                        HeapFile::open_or_repair(&pool, file, meta.layout)?;
                        (
                            HeapStore { file, file_no: meta.file_no },
                            Arc::new(HeapEngine::with_layout(file, meta.layout)),
                        )
                    }
                    EngineKind::Lsm => {
                        let dir = tables_dir.join(format!("{:06}.lsm", meta.file_no));
                        let engine = LsmEngine::open_with_trigger(&dir, LSM_BLOCK_SIZE, config.storage.lsm_compaction_trigger)?;
                        (HeapStore { file: LSM_FILE_ID, file_no: meta.file_no }, Arc::new(engine))
                    }
                };
                catalog.create_table(&meta.name, schema, heap, meta.engine, meta.layout, engine)?;
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
            clog_base = snap.clog_base;
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
            lobs: LobStore::open(&path.join("lobs"))?,
            wal: Wal::open(&wal_path)?,
            data_dir: path.to_path_buf(),
            next_table_file: AtomicU32::new(next_table_file),
            next_index_file: AtomicU32::new(next_index_file),
            trx: TransactionManager::new(next_trx_id, committed_trxs, clog_base),
            wal_checkpoint_threshold: AtomicU64::new(config.wal.checkpoint_threshold),
            conflict: config.transaction.conflict,
            writer: Arc::new(DatabaseWriteLock::new(config.transaction.lock_timeout_ms)),
            locks: LockManager::new(config.transaction.lock_timeout_ms, 100),
            checkpoint_lock: Mutex::new(()),
            catalog_lock: Mutex::new(()),
            _temp: None,
        };
        db.recover_from_wal(&plan, &mut touched)?;

        // never hand a crashed transaction's id to a new transaction
        db.trx.ensure_next_id_at_least(plan.max_trx_id);
        let committed_repaired = plan
            .committed_ids
            .iter()
            .any(|id| !db.trx.is_committed(*id));
        if committed_repaired {
            db.trx.extend_committed(plan.committed_ids.iter().copied());
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

    /// The configured transaction isolation level.
    pub(crate) fn isolation(&self) -> Isolation {
        self.config.transaction.isolation
    }

    /// A fresh snapshot of the currently committed state, used by read
    /// committed to give each statement its own view (and by EPQ restarts).
    pub(crate) fn current_snapshot(&self) -> crate::db::transaction::Snapshot {
        self.trx.snapshot()
    }

    /// Buffer-pool lookup counters, for observability and cache-behavior tests.
    pub fn buffer_pool_stats(&self) -> crate::storage::PoolStats {
        self.pool.stats()
    }

    pub fn flush(&self) -> Result<()> {
        if !self.trx.no_open_transactions() {
            // truncating the log now would drop the open transaction's redo
            // records, so its later COMMIT could not be recovered
            return Err(Error::Runtime(
                "cannot flush while transactions are open".into(),
            ));
        }
        self.flush_inner(None)
    }

    /// Runs a checkpoint. `exclude` is the id of the statement's own
    /// (autocommit) transaction, which must not count as an open one.
    pub(crate) fn flush_inner(&self, exclude: Option<u64>) -> Result<()> {
        let _serial = self.checkpoint_lock.lock();
        // Writing pages back is safe while transactions are open; only
        // dropping the log needs the no-open guarantee below.
        let commits = self.trx.committed_count();
        self.pool.flush_all()?;
        // LSM tables flush their memtable to a durable SSTable; heap tables
        // are covered by the buffer-pool flush above.
        for meta in self.catalog().table_metas() {
            self.catalog().table(&meta.name)?.engine().flush()?;
        }
        self.save_catalog()?;
        // Truncate only if no transaction committed while we flushed (its
        // pages may not be in our snapshot) and none is open (its redo is
        // still needed). Holding the open set across the check and the
        // truncate keeps a begin or a commit from slipping between them.
        self.trx.with_open_set(|open| {
            let others_open = open.iter().any(|&id| Some(id) != exclude);
            if !others_open && self.trx.committed_count() == commits {
                self.wal.truncate()?;
            }
            Ok(())
        })
    }

    /// Physically removes rows no transaction can ever see again:
    /// delete-marked rows whose deleter committed, and orphan versions whose
    /// creator never committed (left behind by a crashed transaction).
    /// Stale index entries of purged rows are removed too. Must run with no
    /// open transactions (the VACUUM statement enforces this).
    pub(crate) fn vacuum(&self) -> Result<usize> {
        let mut purged = 0;
        // VACUUM runs with no open transaction, so the clog is final for every
        // xid: a version is dead if its creator never committed, or its deleter
        // did commit.
        let clog = self.trx.commit_status();
        let metas = self.catalog().table_metas();
        for meta in metas {
            let ops = self.index_ops(&meta.name)?;
            let engine = self.catalog().table(&meta.name)?.engine();
            for (rid, rec) in self.store_scan_raw(&meta.name)? {
                let (creator, deleter, row) = crate::storage::codec::decode_record(&rec, &self.lobs)?;
                let dead = !clog.is_committed(creator)
                    || (deleter != 0 && clog.is_committed(deleter));
                if dead {
                    for (ci, ix_file) in &ops {
                        let key = encode_key(&row[*ci])?;
                        BTree::at(*ix_file).delete(&self.pool, &key, rid)?;
                    }
                    self.free_lob_refs(&rec);
                    engine.delete(&self.pool, rid)?;
                    purged += 1;
                    continue;
                }
                // A live version whose delete marker was left by a transaction
                // that never committed is un-deleted, so the horizon below can
                // treat every old xid as committed.
                if deleter != 0 && !clog.is_committed(deleter) {
                    engine.delete_mark(&self.pool, rid, 0, 0)?;
                }
            }
        }
        // Every xid below the next unallocated one has ended (no transaction is
        // open) and no live version still references an aborted xid, so the
        // clog prefix is frozen and can be dropped, keeping xid state bounded.
        self.trx.advance_horizon(self.trx.next_id());
        self.save_catalog()?;
        Ok(purged)
    }

    /// Replays committed WAL records into the buffer pool (they reach the
    /// disk with the next flush) and rebuilds indexes of touched tables.
    /// `touched` accumulates heap file numbers that must have their indexes
    /// rebuilt; it may arrive pre-seeded with repaired index files.
    fn recover_from_wal(&self, plan: &wal::RecoveryPlan, touched: &mut HashSet<u32>) -> Result<()> {
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

    /// Commit bookkeeping shared by explicit COMMIT and autocommit. Under
    /// first-committer-wins a write transaction whose target rows were changed
    /// by a transaction that committed after its snapshot is rejected before
    /// any commit record is written.
    fn commit_trx(&self, trx: &TrxState, wrote: bool) -> Result<()> {
        let trx_id = trx.id;
        if wrote && self.conflict == ConflictStrategy::Fcw {
            self.check_conflicts(trx)?;
        }
        if !wrote {
            // A read-only transaction created no versioned rows, so no future
            // snapshot needs its id and its bookkeeping need not hit disk.
            // Skipping the catalog rewrite keeps SELECT cheap.
            self.trx.commit(trx_id);
            self.locks.unlock_all(trx_id);
            return Ok(());
        }
        // The synced commit record is the durability point. Flush the
        // statement's buffered rows and the commit record, then sync, all
        // *before* marking the transaction committed in memory, so a logging
        // failure cannot leave a "committed" id whose record is not durable.
        self.wal.append_frames(&trx.wal)?;
        self.wal.append(trx_id, &Record::Commit)?;
        self.wal.sync()?;
        self.trx.commit(trx_id);
        // No per-commit catalog rewrite: the synced WAL already carries this
        // commit, so recovery can rebuild it. The catalog is saved by DDL and
        // by `flush_inner` before it truncates the log, which is what keeps the
        // persisted committed set (and next xid) from lagging behind a
        // truncated log. Rewriting it here made every commit O(committed ids).
        if self.wal.len().unwrap_or(0) > self.wal_checkpoint_threshold.load(Ordering::Relaxed)
            && self.trx.no_open_transactions()
            && let Err(e) = self.flush()
        {
            eprintln!("commit: opportunistic checkpoint failed: {e}");
        }
        self.locks.unlock_all(trx_id);
        Ok(())
    }

    /// First-committer-wins validation: for every base version this
    /// transaction delete-marked, reject when it was modified by a transaction
    /// that committed after our snapshot. Both the marker we overwrote
    /// (`prev_deleter`) and the marker currently on the page are checked, so
    /// the loser is caught regardless of write order.
    fn check_conflicts(&self, trx: &TrxState) -> Result<()> {
        for undo in &trx.undo {
            let (table, rid, prev) = match undo {
                Undo::DeleteMark { table, rid, prev_deleter, .. } => {
                    (table, *rid, *prev_deleter)
                }
                Undo::Update { table, old_rid, prev_deleter, .. } => {
                    (table, *old_rid, *prev_deleter)
                }
                Undo::Insert { .. } => continue,
            };
            if self.conflicting_committer(trx, prev) {
                return Err(conflict_error(table));
            }
            let engine = self.catalog().table(table)?.engine();
            let rec = engine.get(&self.pool, rid)?;
            let (_, current, _) = decode_record(&rec, &self.lobs)?;
            if self.conflicting_committer(trx, current) {
                return Err(conflict_error(table));
            }
        }
        Ok(())
    }

    /// Whether `id` names a transaction that committed after `trx`'s snapshot.
    fn conflicting_committer(&self, trx: &TrxState, id: u64) -> bool {
        id != 0
            && id != trx.id
            && !trx.committed_before(id)
            && self.trx.is_committed(id)
    }

    /// Whether the version at `rid` was deleted or superseded by a transaction
    /// that committed after `trx`'s snapshot. Callers hold the row lock, so the
    /// answer is stable. Read committed restarts the statement on `true` (EPQ);
    /// repeatable read lets the commit-time check abort instead.
    fn row_was_concurrently_modified(
        &self,
        rid: Rid,
        engine: &dyn TableStorage,
        trx: &TrxState,
    ) -> Result<bool> {
        let rec = engine.get(&self.pool, rid)?;
        let (_, deleter, _) = decode_record(&rec, &self.lobs)?;
        Ok(self.conflicting_committer(trx, deleter))
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
                let (snapshot, clog) = self.trx.begin_snapshot();
                session.begin_readonly(snapshot, clog);
            } else {
                let id = self.trx.begin_open();
                let (snapshot, clog) = self.trx.begin_snapshot();
                session.begin(id, snapshot, clog, false);
            }
        }
        let mut out = Vec::new();
        let result = run_plan(self, session, plan, &mut out);
        if autocommit && let Some(mut trx) = session.trx.take() {
            if result.is_ok() {
                if !read_only {
                    self.commit_trx(&trx, false)?;
                }
            } else {
                self.rollback_trx(&mut trx)?;
                if !read_only {
                    self.trx.remove_open(trx.id);
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
                // 2PL: a transaction holds the database write lock from BEGIN
                // (before its snapshot) until COMMIT/ROLLBACK, so writers
                // serialize and later writers build on the latest commit.
                if self.conflict == ConflictStrategy::TwoPl {
                    self.acquire_writer(session.id())?;
                    session.set_holds_writer(true);
                }
                let id = self.trx.begin_open();
                let (snapshot, clog) = self.trx.begin_snapshot();
                session.begin(id, snapshot, clog, true);
                Ok(None)
            }
            crate::ast::Stmt::Trx(crate::ast::TrxCtl::Commit) => {
                if session.trx.is_none() {
                    return Err(Error::Runtime("no active transaction".into()));
                }
                let mut result = Ok(None);
                if let Some(mut trx) = session.trx.take() {
                    let wrote = !trx.undo.is_empty();
                    if let Err(e) = self.commit_trx(&trx, wrote) {
                        // a conflict or log failure aborts this transaction:
                        // undo its work and drop its bookkeeping. Always clear
                        // the open slot, even if the undo itself fails.
                        self.trx.remove_open(trx.id);
                        if let Err(rb) = self.rollback_trx(&mut trx) {
                            result = Err(rb);
                        } else {
                            result = Err(e);
                        }
                    }
                }
                self.end_writer(session);
                result
            }
            crate::ast::Stmt::Trx(crate::ast::TrxCtl::Rollback) => {
                if session.trx.is_none() {
                    return Err(Error::Runtime("no active transaction".into()));
                }
                let mut result: Result<Option<ResultSet>> = Ok(None);
                if let Some(mut trx) = session.trx.take() {
                    self.trx.remove_open(trx.id);
                    result = self.rollback_trx(&mut trx).map(|()| None);
                }
                self.end_writer(session);
                result
            }
            other => {
                let read_only = is_read_only(other);
                let autocommit = session.trx.is_none();
                let took_writer = self.needs_writer(session, read_only);
                if took_writer {
                    self.acquire_writer(session.id())?;
                }
                if autocommit {
                    if read_only {
                        let (snapshot, clog) = self.trx.begin_snapshot();
                        session.begin_readonly(snapshot, clog);
                    } else {
                        let id = self.trx.begin_open();
                        let (snapshot, clog) = self.trx.begin_snapshot();
                        session.begin(id, snapshot, clog, false);
                    }
                } else if self.isolation() == Isolation::ReadCommitted {
                    // read committed: every statement sees the latest committed
                    // data, so refresh the transaction's snapshot per statement.
                    session.trx.as_mut().expect("explicit trx is open").snapshot =
                        self.current_snapshot();
                }
                let (undo_mark, wal_mark) = session
                    .trx
                    .as_ref()
                    .map(|t| (t.undo.len(), t.wal.len()))
                    .unwrap_or((0, 0));
                let mut event = SqlEvent::new(other);
                let outcome = match pipeline.run(self, session, &mut event) {
                    Ok(()) => {
                        let rs = event.result.take().expect("execute stage produced no result");
                        if autocommit
                            && let Some(mut trx) = session.trx.take()
                            && !read_only
                        {
                            let wrote = !trx.undo.is_empty();
                            match self.commit_trx(&trx, wrote) {
                                Ok(()) => Ok(Some(rs)),
                                Err(e) => {
                                    // clear the open slot regardless of whether
                                    // the compensating undo succeeds
                                    self.trx.remove_open(trx.id);
                                    Err(self.rollback_trx(&mut trx).err().unwrap_or(e))
                                }
                            }
                        } else {
                            Ok(Some(rs))
                        }
                    }
                    Err(e) => {
                        // Undo only what the failed statement changed. A
                        // read-only autocommit pseudo-transaction is simply
                        // discarded; an explicit transaction stays open for
                        // retry or rollback with its earlier work intact.
                        if autocommit {
                            if let Some(mut trx) = session.trx.take() {
                                let undone = if read_only {
                                    Ok(())
                                } else {
                                    self.rollback_trx_to(&mut trx, undo_mark, wal_mark)
                                };
                                self.trx.remove_open(trx.id);
                                undone?;
                            }
                        } else if !read_only
                            && let Some(trx) = session.trx.as_mut()
                        {
                            self.rollback_trx_to(trx, undo_mark, wal_mark)?;
                        }
                        Err(e)
                    }
                };
                if took_writer {
                    self.release_writer(session.id());
                }
                outcome
            }
        }
    }

    /// Releases the 2PL write lock at the end of an explicit transaction.
    fn end_writer(&self, session: &mut Session) {
        if session.holds_writer() {
            self.release_writer(session.id());
            session.set_holds_writer(false);
        }
    }

    /// Rolls back any open transaction when a session goes away.
    pub fn rollback_session(&self, session: &mut Session) -> Result<()> {
        let mut result = Ok(());
        if let Some(mut trx) = session.trx.take() {
            self.trx.remove_open(trx.id);
            result = self.rollback_trx(&mut trx);
        }
        self.end_writer(session);
        result
    }

    /// Whether any session other than `trx_id` has a transaction open.
    pub(crate) fn has_open_trxs_excluding(&self, trx_id: u64) -> bool {
        self.trx.has_open_excluding(trx_id)
    }

    /// Overrides the auto-checkpoint log budget in bytes; mainly for tests.
    pub fn set_wal_checkpoint_threshold(&self, bytes: u64) {
        self.wal_checkpoint_threshold.store(bytes, Ordering::Relaxed);
    }

    /// The database's 2PL write lock, so the instance layer can take it before
    /// acquiring the database lock.
    pub(crate) fn write_lock(&self) -> Arc<DatabaseWriteLock> {
        Arc::clone(&self.writer)
    }

    /// 2PL: takes the database write lock for `session_id`.
    fn acquire_writer(&self, session_id: u64) -> Result<()> {
        self.writer.acquire(session_id)
    }

    /// 2PL: releases the database write lock held by `session_id`.
    fn release_writer(&self, session_id: u64) {
        self.writer.release(session_id);
    }

    /// Whether the session needs to take the 2PL write lock for a statement.
    fn needs_writer(&self, session: &Session, read_only: bool) -> bool {
        self.conflict == ConflictStrategy::TwoPl && !read_only && !session.holds_writer()
    }

    fn rollback_trx(&self, trx: &mut TrxState) -> Result<()> {
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
        if undo_mark == 0 {
            // full rollback: release the transaction's row locks
            self.locks.unlock_all(trx.id);
        }
        trx.wal.truncate(wal_mark);
        while trx.undo.len() > undo_mark {
            let undo = trx.undo.pop().expect("len > mark checked");
            match undo {
                Undo::Insert { table, rid, row } => {
                    let engine = self.catalog().table(&table)?.engine();
                    if let Ok(record) = engine.get(&self.pool, rid) {
                        self.free_lob_refs(&record);
                    }
                    engine.delete(&self.pool, rid)?;
                    for (ci, ix_file) in self.index_ops(&table)? {
                        let key = encode_key(&row[ci])?;
                        BTree::at(ix_file).delete(&self.pool, &key, rid)?;
                    }
                }
                Undo::DeleteMark { table, rid, prev_deleter, prev_next_rid } => {
                    let engine = self.catalog().table(&table)?.engine();
                    engine.delete_mark(&self.pool, rid, prev_deleter, prev_next_rid)?;
                }
                Undo::Update { table, old_rid, new_rid, new_row, prev_deleter, prev_next_rid } => {
                    let engine = self.catalog().table(&table)?.engine();
                    if let Ok(record) = engine.get(&self.pool, new_rid) {
                        self.free_lob_refs(&record);
                    }
                    engine.delete(&self.pool, new_rid)?;
                    for (ci, ix_file) in self.index_ops(&table)? {
                        let key = encode_key(&new_row[ci])?;
                        BTree::at(ix_file).delete(&self.pool, &key, new_rid)?;
                    }
                    engine.delete_mark(&self.pool, old_rid, prev_deleter, prev_next_rid)?;
                }
            }
        }
        Ok(())
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
    fn free_lob_refs(&self, record: &[u8]) {
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
        let file_no = self.next_table_file.fetch_add(1, Ordering::SeqCst);
        match kind {
            EngineKind::Heap => {
                let path = self.data_dir.join("tables").join(format!("{file_no:06}.dbf"));
                let file = self.pool.create_file(&path)?;
                HeapFile::init_with_layout(&self.pool, file, layout)?;
                Ok((
                    HeapStore { file, file_no },
                    Arc::new(HeapEngine::with_layout(file, layout)),
                ))
            }
            EngineKind::Lsm => {
                let dir = self.data_dir.join("tables").join(format!("{file_no:06}.lsm"));
                let engine = LsmEngine::open_with_trigger(
                    &dir,
                    LSM_BLOCK_SIZE,
                    self.config.storage.lsm_compaction_trigger,
                )?;
                Ok((HeapStore { file: LSM_FILE_ID, file_no }, Arc::new(engine)))
            }
        }
    }

    pub(crate) fn new_index_heap(&self, _name: &str) -> Result<IndexStore> {
        let file_no = self.next_index_file.fetch_add(1, Ordering::SeqCst);
        let path = self.data_dir.join("indexes").join(format!("{file_no:06}.idxf"));
        let file = self.pool.create_file(&path)?;
        BTree::init(&self.pool, file)?;
        Ok(IndexStore { file, file_no })
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
            let dir = self.data_dir.join("tables").join(format!("{:06}.lsm", dropped.file_no));
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
        let (checks, engine) = {
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
            (checks, catalog.table(table)?.engine())
        };
        if checks.is_empty() {
            return Ok(());
        }
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
                let rec = engine.get(&self.pool, rid)?;
                let (creator, deleter, _) = decode_record(&rec, &self.lobs)?;
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
        trx: &mut TrxState,
    ) -> Result<Rid> {
        let (file_no, engine) = {
            let catalog = self.catalog();
            let t = catalog.table(name)?;
            (t.heap.file_no, t.engine())
        };
        let creator = trx.id;
        let data = encode_record(creator, 0, 0, &row, &self.lobs, self.inline_lob_limit())?;
        let rid = engine.insert(&self.pool, &data)?;
        // Record the undo as soon as the row exists so that a later failure in
        // the WAL or index steps is still undone by the enclosing transaction.
        trx.undo.push(Undo::Insert { table: name.to_string(), rid, row: row.clone() });
        crate::wal::encode_frame_into(
            &mut trx.wal,
            creator,
            &Record::Insert { file_no, rid, record: data },
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
        let (file_no, engine) = {
            let catalog = self.catalog();
            let t = catalog.table(name)?;
            (t.heap.file_no, t.engine())
        };
        let deleter = trx.id;
        for rid in rids {
            // serialize writers of the same row; different rows proceed
            self.locks.lock(deleter, name, *rid)?;
            // read committed: the row changed under us, so restart the statement
            // (EPQ) with a fresh snapshot rather than abort.
            if self.isolation() == Isolation::ReadCommitted
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
                &Record::DeleteMark { file_no, rid: *rid, deleter, next_rid: 0 },
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
        let (file_no, engine) = {
            let catalog = self.catalog();
            let t = catalog.table(name)?;
            (t.heap.file_no, t.engine())
        };
        let trx_id = trx.id;
        let ops = self.index_ops(name)?;
        for (rid, new_row) in updates {
            // serialize writers of the same row; different rows proceed
            self.locks.lock(trx_id, name, *rid)?;
            // read committed: the row changed under us, so restart the statement
            // (EPQ) with a fresh snapshot rather than abort.
            if self.isolation() == Isolation::ReadCommitted
                && self.row_was_concurrently_modified(*rid, &*engine, trx)?
            {
                return Err(Error::Retry);
            }
            let data = encode_record(trx_id, 0, 0, new_row, &self.lobs, self.inline_lob_limit())?;
            let new_rid = engine.insert(&self.pool, &data)?;
            crate::wal::encode_frame_into(
                &mut trx.wal,
                trx_id,
                &Record::Insert { file_no, rid: new_rid, record: data },
            );
            // link the old version forward to the new one (PG's t_ctid)
            let next_rid = crate::storage::codec::pack_rid(new_rid.page_no, new_rid.slot);
            let (prev_deleter, prev_next_rid) =
                engine.delete_mark(&self.pool, *rid, trx_id, next_rid)?;
            crate::wal::encode_frame_into(
                &mut trx.wal,
                trx_id,
                &Record::DeleteMark { file_no, rid: *rid, deleter: trx_id, next_rid },
            );
            for (ci, ix_file) in &ops {
                let key = encode_key(&new_row[*ci])?;
                BTree::at(*ix_file).insert(&self.pool, &key, new_rid)?;
            }
            trx.undo.push(Undo::Update {
                table: name.to_string(),
                old_rid: *rid,
                new_rid,
                new_row: new_row.clone(),
                prev_deleter,
                prev_next_rid,
            });
        }
        Ok(())
    }
}

/// Whether a statement mutates schema or physical state and therefore needs the
/// database in exclusive mode. DML and transaction control take the database
/// shared: concurrent writers are serialized per row by the lock manager.
pub(crate) fn is_exclusive(stmt: &crate::ast::Stmt) -> bool {
    matches!(
        stmt,
        crate::ast::Stmt::CreateIndex(_)
            | crate::ast::Stmt::CreateTable(_)
            | crate::ast::Stmt::CreateView(_)
            | crate::ast::Stmt::DropIndex(_)
            | crate::ast::Stmt::DropTable(_)
            | crate::ast::Stmt::DropView(_)
            | crate::ast::Stmt::Checkpoint
            | crate::ast::Stmt::Vacuum
    )
}

/// Whether a statement only reads, so its autocommit needs no transaction
/// bookkeeping. Everything else (DML, DDL, CHECKPOINT, VACUUM) may mutate
/// state and takes the normal round trip.
pub(crate) fn is_read_only(stmt: &crate::ast::Stmt) -> bool {
    matches!(
        stmt,
        crate::ast::Stmt::Select(_)
            | crate::ast::Stmt::Explain(_)
            | crate::ast::Stmt::ShowTables
            | crate::ast::Stmt::ShowColumns(_)
    )
}

/// A first-committer-wins conflict on `table`, reported with PostgreSQL's
/// serialization-failure wording (`SQLSTATE 40001`): the version this
/// transaction read was updated by another transaction that committed after
/// its snapshot.
pub(crate) fn conflict_error(table: &str) -> Error {
    Error::Runtime(format!(
        "could not serialize access due to concurrent update on {table}"
    ))
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
    if db.config().execution.mode == ExecutionMode::Chunk {
        while let Some(chunk) = plan.next_chunk(&mut ctx)? {
            out.extend(chunk.to_rows());
        }
    } else {
        while let Some(row) = plan.next(&mut ctx)? {
            out.push(row);
        }
    }
    plan.close()?;
    Ok(())
}

fn dir_err(dir: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::Runtime(format!("cannot create dir {}: {e}", dir.display()))
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Runtime(format!("cannot delete file {}: {e}", path.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_autocommit_does_not_touch_bookkeeping() {
        let db = Database::open_in_memory().unwrap();
        let before = db.trx.next_id();

        db.execute_sql("select 1;").unwrap();

        assert_eq!(db.trx.next_id(), before, "read-only must not allocate a trx id");
        assert!(
            db.trx.committed_ids().is_empty(),
            "read-only must not join committed"
        );
        assert!(db.trx.no_open_transactions(), "read-only must not stay open");
    }

    #[test]
    fn write_autocommit_registers_its_transaction() {
        let db = Database::open_in_memory().unwrap();
        db.execute_sql("create table t (id int);").unwrap();
        let before = db.trx.next_id();

        db.execute_sql("insert into t values (1);").unwrap();

        assert!(db.trx.next_id() > before, "write must allocate a trx id");
        assert!(db.trx.is_committed(before), "write must be committed");
        assert!(db.trx.no_open_transactions(), "write must not stay open");
    }

    #[test]
    fn vacuum_advances_the_clog_horizon_and_bounds_xid_state() {
        let db = Database::open_in_memory().unwrap();
        db.execute_sql("create table t (id int);").unwrap();
        db.execute_sql("insert into t values (1), (2), (3);").unwrap();
        db.execute_sql("delete from t where id = 2;").unwrap();
        let before = db.trx.next_id();
        assert!(!db.trx.committed_ids().is_empty(), "commits are tracked");

        db.execute_sql("vacuum;").unwrap();

        // every ended xid is below the horizon and its bit is dropped, so the
        // clog no longer grows with the number of past commits
        assert!(db.trx.clog_base() >= before, "horizon covers all ended xids");
        assert!(db.trx.committed_ids().is_empty(), "the clog prefix is dropped");
        // the older committed rows stay visible through the frozen horizon
        let out = db.execute_sql("select id from t order by id;").unwrap();
        let ResultSet::Rows { rows, .. } = out.into_iter().next().unwrap() else {
            panic!("expected rows");
        };
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(3)]]);
    }

    #[test]
    fn update_links_the_old_version_to_the_new_one() {
        use crate::storage::codec::{decode_record, record_next_rid, unpack_rid};
        let db = Database::open_in_memory().unwrap();
        db.execute_sql("create table t (id int, v int);").unwrap();
        db.execute_sql("insert into t values (1, 10);").unwrap();
        db.execute_sql("update t set v = 20 where id = 1;").unwrap();

        let recs = db.store_scan_raw("t").unwrap();
        assert_eq!(recs.len(), 2, "both versions remain");
        let (old_rid, old_rec, next) = recs
            .iter()
            .find_map(|(rid, rec)| {
                let next = record_next_rid(rec).unwrap();
                (next != 0).then(|| (*rid, rec.clone(), next))
            })
            .expect("exactly one version links forward");
        let (_, _, old_row) = decode_record(&old_rec, db.lobs()).unwrap();
        assert_eq!(old_row, vec![Value::Int(1), Value::Int(10)]);

        let (page, slot) = unpack_rid(next);
        assert_ne!((old_rid.page_no, old_rid.slot), (page, slot));
        let (_, _, new_row) = decode_record(
            &recs
                .iter()
                .find(|(r, _)| r.page_no == page && r.slot == slot)
                .expect("the pointer targets the new version")
                .1,
            db.lobs(),
        )
        .unwrap();
        assert_eq!(new_row, vec![Value::Int(1), Value::Int(20)]);
    }
}
