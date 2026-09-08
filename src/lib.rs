pub mod ast;
pub mod catalog;
mod error;
pub mod exec;
pub mod lexer;
pub mod parser;
mod repl;
pub mod result;
pub mod storage;
pub mod value;

pub use error::{Error, Result};
pub use repl::run_repl;
pub use result::ResultSet;

use crate::catalog::{Catalog, RowStore};
use crate::storage::codec::encode_row;
use crate::storage::{BufferPool, HeapFile, Rid};
use crate::value::Value;

pub struct Database {
    catalog: Catalog,
    pool: Option<BufferPool>,
}

impl Database {
    pub fn open_in_memory() -> Self {
        Self { catalog: Catalog::default(), pool: None }
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

    pub(crate) fn store_scan(&self, name: &str) -> Result<Vec<(Rid, Vec<Value>)>> {
        match &self.catalog.table(name)?.store {
            RowStore::Mem(rows) => Ok(rows
                .iter()
                .enumerate()
                .map(|(i, row)| (Rid::new(0, i as u16), row.clone()))
                .collect()),
            RowStore::Heap { .. } => Err(Error::Runtime("heap store not wired yet".into())),
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
            RowStore::Heap { .. } => Err(Error::Runtime("heap store not wired yet".into())),
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
            RowStore::Heap { .. } => Err(Error::Runtime("heap store not wired yet".into())),
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
            RowStore::Heap { .. } => Err(Error::Runtime("heap store not wired yet".into())),
        }
    }

    fn heap_file(&self, name: &str) -> Result<Option<crate::storage::FileId>> {
        match &self.catalog.table(name)?.store {
            RowStore::Heap { file } => Ok(Some(*file)),
            RowStore::Mem(_) => Ok(None),
        }
    }
}
