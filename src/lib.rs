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
use crate::catalog::{Catalog, ColumnDesc, RowStore, Schema};
use crate::storage::codec::{decode_row, encode_row};
use crate::storage::{BufferPool, DiskManager, FileId, HeapFile, Rid};
use crate::value::Value;

pub const BUFFER_POOL_FRAMES: usize = 64;

pub struct Database {
    catalog: Catalog,
    pool: Option<BufferPool>,
    data_dir: Option<PathBuf>,
    next_table_file: u32,
}

impl Database {
    pub fn open_in_memory() -> Self {
        Self {
            catalog: Catalog::default(),
            pool: None,
            data_dir: None,
            next_table_file: 0,
        }
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
                    RowStore::Heap { file, file_no: meta.file_no },
                )?;
            }
            next_table_file = snap.next_table_file;
        }

        Ok(Self {
            catalog,
            pool: Some(pool),
            data_dir: Some(path.to_path_buf()),
            next_table_file,
        })
    }

    pub fn flush(&mut self) -> Result<()> {
        if let Some(pool) = &mut self.pool {
            pool.flush_all()?;
        }
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

    pub(crate) fn new_table_store(&mut self, _name: &str) -> Result<RowStore> {
        let (Some(pool), Some(dir)) = (&mut self.pool, &self.data_dir) else {
            return Ok(RowStore::Mem(vec![]));
        };
        let file_no = self.next_table_file;
        self.next_table_file += 1;
        let path = dir.join("tables").join(format!("{file_no:06}.dbf"));
        let file = pool.create_file(&path)?;
        HeapFile::init(pool, file)?;
        Ok(RowStore::Heap { file, file_no })
    }

    pub(crate) fn save_catalog(&self) -> Result<()> {
        let Some(dir) = &self.data_dir else {
            return Ok(());
        };
        let snap = CatalogSnapshot {
            next_table_file: self.next_table_file,
            tables: self.catalog.table_metas(),
        };
        let bytes = encode_catalog(&snap);
        std::fs::write(dir.join("catalog.bin"), bytes)
            .map_err(|e| Error::Runtime(format!("cannot write catalog: {e}")))
    }

    pub(crate) fn store_scan(&mut self, name: &str) -> Result<Vec<(Rid, Vec<Value>)>> {
        let file = self.heap_file(name)?;
        if let Some(file) = file {
            let heap = HeapFile::at(file);
            let pool = self.pool.as_mut().unwrap();
            let mut out = Vec::new();
            heap.for_each(pool, |rid, rec| {
                let (row, _) = decode_row(rec)?;
                out.push((rid, row));
                Ok(())
            })?;
            return Ok(out);
        }
        match &self.catalog.table(name)?.store {
            RowStore::Mem(rows) => Ok(rows
                .iter()
                .enumerate()
                .map(|(i, row)| (Rid::new(0, i as u16), row.clone()))
                .collect()),
            RowStore::Heap { .. } => Err(Error::Runtime("inconsistent store".into())),
        }
    }

    pub(crate) fn store_insert(&mut self, name: &str, row: Vec<Value>) -> Result<Rid> {
        let file = self.heap_file(name)?;
        if let Some(file) = file {
            let heap = HeapFile::at(file);
            let data = encode_row(&row);
            return heap.insert(self.pool.as_mut().unwrap(), &data);
        }
        match &mut self.catalog.table_mut(name)?.store {
            RowStore::Mem(rows) => {
                rows.push(row);
                Ok(Rid::new(0, (rows.len() - 1) as u16))
            }
            RowStore::Heap { .. } => Err(Error::Runtime("inconsistent store".into())),
        }
    }

    pub(crate) fn store_delete_all(&mut self, name: &str, rids: &[Rid]) -> Result<()> {
        let file = self.heap_file(name)?;
        if let Some(file) = file {
            let heap = HeapFile::at(file);
            let pool = self.pool.as_mut().unwrap();
            for rid in rids {
                heap.delete(pool, *rid)?;
            }
            return Ok(());
        }
        let drop: std::collections::HashSet<u16> = rids.iter().map(|r| r.slot).collect();
        match &mut self.catalog.table_mut(name)?.store {
            RowStore::Mem(rows) => {
                let mut i = 0;
                rows.retain(|_| {
                    let keep = !drop.contains(&(i as u16));
                    i += 1;
                    keep
                });
                Ok(())
            }
            RowStore::Heap { .. } => Err(Error::Runtime("inconsistent store".into())),
        }
    }

    pub(crate) fn store_replace_all(
        &mut self,
        name: &str,
        updates: Vec<(Rid, Vec<Value>)>,
    ) -> Result<()> {
        let file = self.heap_file(name)?;
        if let Some(file) = file {
            let heap = HeapFile::at(file);
            let pool = self.pool.as_mut().unwrap();
            for (rid, row) in updates {
                let data = encode_row(&row);
                heap.delete(pool, rid)?;
                heap.insert(pool, &data)?;
            }
            return Ok(());
        }
        let mut updates = updates;
        updates.sort_by_key(|(rid, _)| rid.slot);
        match &mut self.catalog.table_mut(name)?.store {
            RowStore::Mem(rows) => {
                for (rid, row) in updates {
                    rows[rid.slot as usize] = row;
                }
                Ok(())
            }
            RowStore::Heap { .. } => Err(Error::Runtime("inconsistent store".into())),
        }
    }

    fn heap_file(&self, name: &str) -> Result<Option<FileId>> {
        match &self.catalog.table(name)?.store {
            RowStore::Heap { file, .. } => Ok(Some(*file)),
            RowStore::Mem(_) => Ok(None),
        }
    }
}
