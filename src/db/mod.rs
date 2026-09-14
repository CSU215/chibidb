//! The `Database` facade: catalog, storage, transactions and WAL.

mod checkpoint;
mod recovery;
mod store;
mod unique;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};

use crate::catalog::meta::decode_catalog;
use crate::catalog::{Catalog, ColumnDesc, HeapStore, IndexStore, Schema};
use crate::config::{Config, EngineKind, ExecutionMode, Isolation};
use crate::error::{Error, Result};
use crate::index::BTree;
use crate::sql::ast::Stmt;
use crate::sql::parser;
use crate::sql::result::ResultSet;
use crate::storage::codec::decode_record;
use crate::storage::engine::{HeapEngine, TableStorage};
use crate::storage::lsm::engine::{LsmEngine, LSM_FILE_ID};
use crate::storage::{BufferPool, DiskManager, HeapFile, LobStore, Rid};
use crate::txn::lock::LockManager;
use crate::txn::transaction::TransactionManager;
use crate::txn::trx::{Session, TrxState, Undo};
use crate::value::Value;
use crate::wal::{self, Record, Wal};

/// SSTable block size for LSM-backed tables.
const LSM_BLOCK_SIZE: usize = 4096;

pub struct Database {
    config: Config,
    catalog: RwLock<Catalog>,
    pub(crate) pool: BufferPool,
    /// Out-of-line storage for long string values.
    lobs: LobStore,
    wal: Wal,
    data_dir: PathBuf,
    next_table_file: AtomicU32,
    next_index_file: AtomicU32,
    /// Transaction id source, committed set and open set.
    trx: TransactionManager,
    wal_checkpoint_threshold: AtomicU64,
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
        let pool = BufferPool::new_with_observability(
            disk,
            config.storage.buffer_pool_frames,
            config.storage.eviction,
            config.observability.clone(),
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
            trx: TransactionManager::new(
                next_trx_id,
                committed_trxs,
                clog_base,
                config.transaction.isolation == Isolation::Serializable,
            ),
            wal_checkpoint_threshold: AtomicU64::new(config.wal.checkpoint_threshold),
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
    pub(crate) fn current_snapshot(&self) -> crate::txn::transaction::Snapshot {
        self.trx.snapshot()
    }

    /// Records that transaction `id` read `table` (serializable conflict
    /// tracking; a no-op at other isolation levels).
    pub(crate) fn note_read(&self, id: u64, table: &str) {
        self.trx.note_read(id, table);
    }

    /// Records that transaction `id` wrote `table`.
    pub(crate) fn note_write(&self, id: u64, table: &str) {
        self.trx.note_write(id, table);
    }

    /// Commit bookkeeping shared by explicit COMMIT and autocommit.
    ///
    /// Under snapshot isolation (repeatable read / serializable) a write
    /// transaction whose target rows were changed by a transaction that
    /// committed after its snapshot is rejected before any commit record is
    /// written; read committed resolves that with EPQ instead, so it is not
    /// checked here.
    fn commit_trx(&self, trx: &TrxState, wrote: bool) -> Result<()> {
        let trx_id = trx.id;
        // Serializable: abort a transaction that would close a cycle of
        // rw-antidependencies. Checked before the graph is dropped for this id.
        if self.trx.ssi_conflict(trx_id) {
            return Err(serialization_error());
        }
        if wrote && self.isolation() != Isolation::ReadCommitted {
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
    pub(crate) fn row_was_concurrently_modified(
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
        stmt: &Stmt,
    ) -> Result<Option<ResultSet>> {
        match stmt {
            Stmt::Trx(crate::sql::ast::TrxCtl::Begin) => {
                if session.trx.is_some() {
                    return Err(Error::Runtime("transaction already begun".into()));
                }
                let id = self.trx.begin_open();
                let (snapshot, clog) = self.trx.begin_snapshot();
                session.begin(id, snapshot, clog, true);
                Ok(None)
            }
            Stmt::Trx(crate::sql::ast::TrxCtl::Commit) => {
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
                result
            }
            Stmt::Trx(crate::sql::ast::TrxCtl::Rollback) => {
                if session.trx.is_none() {
                    return Err(Error::Runtime("no active transaction".into()));
                }
                let mut result: Result<Option<ResultSet>> = Ok(None);
                if let Some(mut trx) = session.trx.take() {
                    self.trx.remove_open(trx.id);
                    result = self.rollback_trx(&mut trx).map(|()| None);
                }
                result
            }
            other => {
                let read_only = other.is_read_only();
                let autocommit = session.trx.is_none();
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
                match self.resolve_optimize_execute(session, other) {
                    Ok(rs) => {
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
                }
            }
        }
    }

    /// Resolves, optimizes and executes one non-transaction-control statement.
    ///
    /// This inlines the former resolve/optimize/execute pipeline stages:
    /// referenced tables are validated against the catalog, a SELECT's access
    /// path is planned, the physical operator tree is built and run when it
    /// covers the statement, and otherwise the statement falls back to the
    /// executor.
    fn resolve_optimize_execute(
        &self,
        session: &mut Session,
        stmt: &Stmt,
    ) -> Result<ResultSet> {
        for table in referenced_tables(stmt) {
            let known = self.table_exists(&table) || self.catalog().view(&table).is_some();
            if !known {
                return Err(Error::Runtime(format!("no such table: {table}")));
            }
        }
        if let Stmt::Select(select) = stmt {
            // The plan is not consumed directly; computing it validates the
            // access path exactly as the optimizer stage did.
            let _ = crate::exec::plan::plan_select(self, select)?;
        }
        let mut physical = crate::exec::operator::build_statement(self, stmt)?;
        if let Some(plan) = physical.as_mut() {
            let kind = plan.output_kind();
            let columns: Vec<String> =
                plan.schema().columns.iter().map(|c| c.name.clone()).collect();
            let rows = self.collect_plan(session, plan.as_mut())?;
            match kind {
                crate::exec::operator::OutputKind::Command => match plan.affected_rows() {
                    Some(n) => Ok(ResultSet::Affected(n)),
                    None => Ok(ResultSet::Message("SUCCESS".into())),
                },
                crate::exec::operator::OutputKind::Rows => Ok(ResultSet::Rows { columns, rows }),
            }
        } else {
            crate::exec::execute(self, session.trx(), stmt)
        }
    }

    /// Rolls back any open transaction when a session goes away.
    pub fn rollback_session(&self, session: &mut Session) -> Result<()> {
        let mut result = Ok(());
        if let Some(mut trx) = session.trx.take() {
            self.trx.remove_open(trx.id);
            result = self.rollback_trx(&mut trx);
        }
        result
    }

    /// Whether any session other than `trx_id` has a transaction open.
    pub(crate) fn has_open_trxs_excluding(&self, trx_id: u64) -> bool {
        self.trx.has_open_excluding(trx_id)
    }
}

/// Top-level tables a statement reads or writes, used for resolution.
fn referenced_tables(stmt: &Stmt) -> Vec<String> {
    match stmt {
        Stmt::Select(s) => s.from.iter().map(|t| t.name.clone()).collect(),
        Stmt::ShowColumns(c) => vec![c.table.clone()],
        Stmt::Insert(i) => vec![i.table.clone()],
        Stmt::Update(u) => vec![u.table.clone()],
        Stmt::Delete(d) => vec![d.table.clone()],
        _ => Vec::new(),
    }
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

/// A serializable-isolation conflict (`SQLSTATE 40001`): committing this
/// transaction would close a cycle of read/write dependencies.
pub(crate) fn serialization_error() -> Error {
    Error::Runtime(
        "could not serialize access due to read/write dependencies among transactions".into(),
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

    #[test]
    fn update_version_link_is_rebuilt_from_the_wal() {
        use crate::storage::codec::{record_next_rid, unpack_rid};
        let dir = tempfile::tempdir().unwrap();
        {
            let db = Database::open(dir.path()).unwrap();
            db.execute_sql("create table t (id int, v int);").unwrap();
            db.execute_sql("insert into t values (1, 10);").unwrap();
            // make the base version durable so recovery only has the update to redo
            db.flush().unwrap();
            db.execute_sql("update t set v = 20 where id = 1;").unwrap();
            db.simulate_crash();
        }
        let db = Database::open(dir.path()).unwrap();
        let recs = db.store_scan_raw("t").unwrap();
        let next = recs
            .iter()
            .map(|(_, r)| record_next_rid(r).unwrap())
            .find(|n| *n != 0)
            .expect("the forwarded link is rebuilt from the WAL DeleteMark frame");
        let (page, slot) = unpack_rid(next);
        assert!(
            recs.iter().any(|(rid, _)| rid.page_no == page && rid.slot == slot),
            "the link must target a version present after recovery"
        );
    }
}
