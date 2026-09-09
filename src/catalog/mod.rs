use std::collections::BTreeMap;

use crate::ast::DataType;
use crate::storage::FileId;
use crate::{Error, Result};

pub mod meta;

use meta::{IndexMeta, TableMeta};

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDesc {
    /// Owning table name or alias; set only in query-time join schemas.
    pub owner: Option<String>,
    pub name: String,
    pub dtype: DataType,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Schema {
    pub columns: Vec<ColumnDesc>,
}

impl Schema {
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    pub fn resolve(&self, qual: Option<&str>, name: &str) -> Result<usize> {
        match qual {
            Some(q) => self
                .columns
                .iter()
                .position(|c| c.owner.as_deref() == Some(q) && c.name == name)
                .ok_or_else(|| Error::Runtime(format!("no such column: {q}.{name}"))),
            None => {
                let matches: Vec<usize> = self
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.name == name)
                    .map(|(i, _)| i)
                    .collect();
                match matches.len() {
                    0 => Err(Error::Runtime(format!("no such column: {name}"))),
                    1 => Ok(matches[0]),
                    _ => Err(Error::Runtime(format!("ambiguous column: {name}"))),
                }
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct HeapStore {
    pub file: FileId,
    pub file_no: u32,
}

#[derive(Debug)]
pub(crate) struct IndexStore {
    pub file: FileId,
    pub file_no: u32,
}

#[derive(Debug)]
pub(crate) struct IndexEntry {
    pub name: String,
    pub table: String,
    pub column: String,
    pub store: IndexStore,
}

#[derive(Debug)]
pub struct Table {
    pub schema: Schema,
    pub(crate) heap: HeapStore,
}

#[derive(Debug, Default)]
pub struct Catalog {
    tables: BTreeMap<String, Table>,
    indexes: BTreeMap<String, IndexEntry>,
}

impl Catalog {
    pub(crate) fn create_table(
        &mut self,
        name: &str,
        schema: Schema,
        heap: HeapStore,
    ) -> Result<()> {
        if self.tables.contains_key(name) {
            return Err(Error::Runtime(format!("table already exists: {name}")));
        }
        self.tables.insert(name.to_string(), Table { schema, heap });
        Ok(())
    }

    pub fn table(&self, name: &str) -> Result<&Table> {
        self.tables
            .get(name)
            .ok_or_else(|| Error::Runtime(format!("no such table: {name}")))
    }

    pub fn table_mut(&mut self, name: &str) -> Result<&mut Table> {
        self.tables
            .get_mut(name)
            .ok_or_else(|| Error::Runtime(format!("no such table: {name}")))
    }

    pub(crate) fn table_metas(&self) -> Vec<TableMeta> {
        self.tables
            .iter()
            .map(|(name, t)| {
                let columns = t
                    .schema
                    .columns
                    .iter()
                    .map(|c| (c.name.clone(), c.dtype))
                    .collect();
                TableMeta { name: name.clone(), columns, file_no: t.heap.file_no }
            })
            .collect()
    }

    pub(crate) fn create_index(
        &mut self,
        name: &str,
        table: String,
        column: String,
        store: IndexStore,
    ) -> Result<()> {
        if self.indexes.contains_key(name) {
            return Err(Error::Runtime(format!("index already exists: {name}")));
        }
        self.indexes.insert(
            name.to_string(),
            IndexEntry { name: name.to_string(), table, column, store },
        );
        Ok(())
    }

    pub(crate) fn drop_index(&mut self, name: &str) -> Result<()> {
        self.indexes
            .remove(name)
            .ok_or_else(|| Error::Runtime(format!("no such index: {name}")))?;
        Ok(())
    }

    pub(crate) fn index_metas(&self) -> Vec<IndexMeta> {
        self.indexes
            .values()
            .map(|ix| IndexMeta {
                name: ix.name.clone(),
                table: ix.table.clone(),
                column: ix.column.clone(),
                file_no: ix.store.file_no,
            })
            .collect()
    }

    pub(crate) fn indexes_for(&self, table: &str) -> Vec<&IndexEntry> {
        self.indexes.values().filter(|ix| ix.table == table).collect()
    }
}
