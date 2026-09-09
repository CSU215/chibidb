pub mod ast;
pub mod catalog;
pub mod datetime;
mod error;
pub mod exec;
pub mod index;
pub mod lexer;
pub mod parser;
mod repl;
pub mod result;
pub mod storage;
pub mod value;

pub use error::{Error, Result};
pub use repl::run_repl;
pub use result::ResultSet;

use std::path::{Path, PathBuf};

use crate::catalog::meta::{decode_catalog, encode_catalog, CatalogSnapshot};
use crate::catalog::{Catalog, ColumnDesc, HeapStore, Schema};
use crate::storage::codec::{decode_row, encode_row};
use crate::storage::{BufferPool, DiskManager, HeapFile, Rid};
use crate::value::Value;

pub const BUFFER_POOL_FRAMES: usize = 64;

pub struct Database {
    catalog: Catalog,
    pool: BufferPool,
    data_dir: PathBuf,
    next_table_file: u32,
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
        std::fs::create_dir_all(&tables_dir).map_err(|e| {
            Error::Runtime(format!("cannot create dir {}: {e}", tables_dir.display()))
        })?;
        let mut pool = BufferPool::new(DiskManager::new(), BUFFER_POOL_FRAMES);
        let mut catalog = Catalog::default();
        let mut next_table_file = 0;

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
            next_table_file = snap.next_table_file;
        }

        Ok(Self {
            catalog,
            pool,
            data_dir: path.to_path_buf(),
            next_table_file,
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

    pub(crate) fn save_catalog(&self) -> Result<()> {
        let snap = CatalogSnapshot {
            next_table_file: self.next_table_file,
            tables: self.catalog.table_metas(),
        };
        let bytes = encode_catalog(&snap);
        std::fs::write(self.data_dir.join("catalog.bin"), bytes)
            .map_err(|e| Error::Runtime(format!("cannot write catalog: {e}")))
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
        heap.insert(&mut self.pool, &data)
    }

    pub(crate) fn store_delete_all(&mut self, name: &str, rids: &[Rid]) -> Result<()> {
        let file = self.catalog.table(name)?.heap.file;
        let heap = HeapFile::at(file);
        for rid in rids {
            heap.delete(&mut self.pool, *rid)?;
        }
        Ok(())
    }

    pub(crate) fn store_replace_all(
        &mut self,
        name: &str,
        updates: Vec<(Rid, Vec<Value>)>,
    ) -> Result<()> {
        let file = self.catalog.table(name)?.heap.file;
        let heap = HeapFile::at(file);
        for (rid, row) in updates {
            let data = encode_row(&row);
            heap.delete(&mut self.pool, rid)?;
            heap.insert(&mut self.pool, &data)?;
        }
        Ok(())
    }
}
