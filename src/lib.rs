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
use crate::storage::{BufferPool, DiskManager, FileId, HeapFile, Rid};
use crate::trx::{TrxState, Undo};
use crate::value::Value;

pub use crate::trx::Session;

pub const BUFFER_POOL_FRAMES: usize = 64;

pub struct Database {
    catalog: Catalog,
    pool: BufferPool,
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

        let catalog_path = path.join("catalog.bin");
        if catalog_path.exists() {
            let bytes = std::fs::read(&catalog_path)
                .map_err(|e| Error::Runtime(format!("cannot read catalog: {e}")))?;
            let snap = decode_catalog(&bytes)?;
            for meta in &snap.tables {
                let fpath = tables_dir.join(format!("{:06}.dbf", meta.file_no));
                let file = pool.open_file(&fpath)?;
                HeapFile::open(&mut pool, file)?;
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
                BTree::open(&mut pool, file)?;
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

        Ok(Self {
            catalog,
            pool,
            data_dir: path.to_path_buf(),
            next_table_file,
            next_index_file,
            next_trx_id,
            committed_trxs,
            _temp: None,
        })
    }

    pub fn flush(&mut self) -> Result<()> {
        self.pool.flush_all()?;
        self.save_catalog()
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
                        self.committed_trxs.insert(trx.id);
                        // commit durability: the committed set (and thus
                        // visibility of the transaction's rows) is persisted
                        self.save_catalog()?;
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
                                    self.committed_trxs.insert(trx.id);
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
        let file = self.catalog.table(name)?.heap.file;
        let heap = HeapFile::at(file);
        let data = encode_record(creator, 0, &row);
        let rid = heap.insert(&mut self.pool, &data)?;
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
        let file = self.catalog.table(name)?.heap.file;
        let heap = HeapFile::at(file);
        for rid in rids {
            heap.delete_mark(&mut self.pool, *rid, deleter)?;
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
        let file = self.catalog.table(name)?.heap.file;
        let heap = HeapFile::at(file);
        let ops = self.index_ops(name)?;
        let mut new_rids = Vec::with_capacity(updates.len());
        for (rid, new_row) in updates {
            heap.delete_mark(&mut self.pool, *rid, trx_id)?;
            let data = encode_record(trx_id, 0, new_row);
            let new_rid = heap.insert(&mut self.pool, &data)?;
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
