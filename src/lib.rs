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
pub mod value;
pub mod wire;

pub use error::{Error, Result};
pub use repl::run_repl;
pub use result::ResultSet;

use std::path::{Path, PathBuf};

use crate::catalog::meta::{decode_catalog, encode_catalog, CatalogSnapshot};
use crate::catalog::{Catalog, ColumnDesc, HeapStore, IndexStore, Schema};
use crate::index::{encode_key, BTree};
use crate::storage::codec::{decode_row, encode_row};
use crate::storage::{BufferPool, DiskManager, FileId, HeapFile, Rid};
use crate::value::Value;

pub const BUFFER_POOL_FRAMES: usize = 64;

pub struct Database {
    catalog: Catalog,
    pool: BufferPool,
    data_dir: PathBuf,
    next_table_file: u32,
    next_index_file: u32,
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
        }

        Ok(Self {
            catalog,
            pool,
            data_dir: path.to_path_buf(),
            next_table_file,
            next_index_file,
            _temp: None,
        })
    }

    pub fn flush(&mut self) -> Result<()> {
        self.pool.flush_all()?;
        self.save_catalog()
    }

    pub fn execute_sql(&mut self, sql: &str) -> Result<Vec<ResultSet>> {
        let stmts = parser::parse(sql)?;
        let mut out = Vec::with_capacity(stmts.len());
        for stmt in &stmts {
            out.push(exec::execute(self, stmt)?);
        }
        Ok(out)
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

    pub(crate) fn store_scan(&mut self, name: &str) -> Result<Vec<(Rid, Vec<Value>)>> {
        let file = self.catalog.table(name)?.heap.file;
        let heap = HeapFile::at(file);
        let mut out = Vec::new();
        heap.for_each(&mut self.pool, |rid, rec| {
            let (row, _) = decode_row(rec)?;
            out.push((rid, row));
            Ok(())
        })?;
        Ok(out)
    }

    pub(crate) fn store_insert(&mut self, name: &str, row: Vec<Value>) -> Result<Rid> {
        let file = self.catalog.table(name)?.heap.file;
        let heap = HeapFile::at(file);
        let data = encode_row(&row);
        let rid = heap.insert(&mut self.pool, &data)?;
        for (ci, ix_file) in self.index_ops(name)? {
            let key = encode_key(&row[ci])?;
            BTree::at(ix_file).insert(&mut self.pool, &key, rid)?;
        }
        Ok(rid)
    }

    pub(crate) fn store_get_rows(
        &mut self,
        name: &str,
        rids: &[Rid],
    ) -> Result<Vec<(Rid, Vec<Value>)>> {
        let file = self.catalog.table(name)?.heap.file;
        let heap = HeapFile::at(file);
        let mut out = Vec::new();
        for rid in rids {
            let rec = heap.get(&mut self.pool, *rid)?;
            let (row, _) = decode_row(&rec)?;
            out.push((*rid, row));
        }
        Ok(out)
    }

    pub(crate) fn store_delete_all(
        &mut self,
        name: &str,
        victims: &[(Rid, Vec<Value>)],
    ) -> Result<()> {
        let file = self.catalog.table(name)?.heap.file;
        let heap = HeapFile::at(file);
        for (rid, row) in victims {
            heap.delete(&mut self.pool, *rid)?;
            for (ci, ix_file) in self.index_ops(name)? {
                let key = encode_key(&row[ci])?;
                BTree::at(ix_file).delete(&mut self.pool, &key, *rid)?;
            }
        }
        Ok(())
    }

    pub(crate) fn store_replace_all(
        &mut self,
        name: &str,
        updates: &[(Rid, Vec<Value>, Vec<Value>)],
    ) -> Result<()> {
        let file = self.catalog.table(name)?.heap.file;
        let heap = HeapFile::at(file);
        let ops = self.index_ops(name)?;
        for (rid, old_row, new_row) in updates {
            heap.delete(&mut self.pool, *rid)?;
            let data = encode_row(new_row);
            let new_rid = heap.insert(&mut self.pool, &data)?;
            for (ci, ix_file) in &ops {
                let old_key = encode_key(&old_row[*ci])?;
                let new_key = encode_key(&new_row[*ci])?;
                let btree = BTree::at(*ix_file);
                btree.delete(&mut self.pool, &old_key, *rid)?;
                btree.insert(&mut self.pool, &new_key, new_rid)?;
            }
        }
        Ok(())
    }
}

fn dir_err(dir: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::Runtime(format!("cannot create dir {}: {e}", dir.display()))
}
