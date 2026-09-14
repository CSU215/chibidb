use std::collections::BTreeMap;
use std::sync::Arc;

use crate::value::DataType;
use crate::config::{EngineKind, PageLayout};
use crate::storage::engine::TableStorage;
use crate::storage::FileId;
use crate::value::Value;
use crate::{Error, Result};

pub mod meta;

use meta::{ColumnMeta, IndexMeta, TableMeta, ViewMeta};

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDesc {
    /// Owning table name or alias; set only in query-time join schemas.
    pub owner: Option<String>,
    pub name: String,
    pub dtype: DataType,
    pub not_null: bool,
    pub primary_key: bool,
    pub unique: bool,
    pub default: Option<Value>,
}

impl ColumnDesc {
    /// A query-time column with no storage constraints attached.
    pub fn plain(owner: Option<String>, name: String, dtype: DataType) -> Self {
        ColumnDesc {
            owner,
            name,
            dtype,
            not_null: false,
            primary_key: false,
            unique: false,
            default: None,
        }
    }
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

/// Files to remove from disk after DROP TABLE.
pub(crate) struct DroppedTable {
    pub heap_file: FileId,
    pub file_no: u32,
    pub engine: EngineKind,
    pub index_files: Vec<FileId>,
}

#[derive(Debug)]
pub(crate) struct IndexEntry {
    pub name: String,
    pub table: String,
    pub column: String,
    /// Backs a PRIMARY KEY / UNIQUE constraint; DML enforces it and it may
    /// not be dropped on its own.
    pub unique: bool,
    pub store: IndexStore,
}

#[derive(Debug)]
pub struct Table {
    pub schema: Schema,
    pub(crate) heap: HeapStore,
    /// The storage engine backing this table (heap or LSM).
    pub(crate) engine: Arc<dyn TableStorage>,
    pub(crate) engine_kind: EngineKind,
    /// Physical page layout; `Pax` only applies to heap tables.
    pub(crate) layout: PageLayout,
}

impl Table {
    /// A shared handle to this table's storage engine.
    pub(crate) fn engine(&self) -> Arc<dyn TableStorage> {
        Arc::clone(&self.engine)
    }
}

#[derive(Debug, Default)]
pub struct Catalog {
    tables: BTreeMap<String, Table>,
    indexes: BTreeMap<String, IndexEntry>,
    views: BTreeMap<String, String>,
}

impl Catalog {
    pub(crate) fn create_table(
        &mut self,
        name: &str,
        schema: Schema,
        heap: HeapStore,
        engine_kind: EngineKind,
        layout: PageLayout,
        engine: Arc<dyn TableStorage>,
    ) -> Result<()> {
        if self.tables.contains_key(name) || self.views.contains_key(name) {
            return Err(Error::Runtime(format!("table already exists: {name}")));
        }
        self.tables.insert(
            name.to_string(),
            Table { schema, heap, engine, engine_kind, layout },
        );
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
                    .map(|c| ColumnMeta {
                        name: c.name.clone(),
                        dtype: c.dtype,
                        not_null: c.not_null,
                        primary_key: c.primary_key,
                        unique: c.unique,
                        default: c.default.clone(),
                    })
                    .collect();
                TableMeta {
                    name: name.clone(),
                    columns,
                    file_no: t.heap.file_no,
                    engine: t.engine_kind,
                    layout: t.layout,
                }
            })
            .collect()
    }

    pub(crate) fn create_index(
        &mut self,
        name: &str,
        table: String,
        column: String,
        unique: bool,
        store: IndexStore,
    ) -> Result<()> {
        if self.indexes.contains_key(name) {
            return Err(Error::Runtime(format!("index already exists: {name}")));
        }
        self.indexes.insert(
            name.to_string(),
            IndexEntry { name: name.to_string(), table, column, unique, store },
        );
        Ok(())
    }

    pub(crate) fn drop_index(&mut self, name: &str) -> Result<()> {
        self.indexes
            .remove(name)
            .ok_or_else(|| Error::Runtime(format!("no such index: {name}")))?;
        Ok(())
    }

    /// Metadata of everything a DROP TABLE must clean up on disk.
    pub(crate) fn drop_table(&mut self, name: &str) -> Result<DroppedTable> {
        let table = self
            .tables
            .remove(name)
            .ok_or_else(|| Error::Runtime(format!("no such table: {name}")))?;
        let ix_names: Vec<String> = self
            .indexes
            .values()
            .filter(|ix| ix.table == name)
            .map(|ix| ix.name.clone())
            .collect();
        let mut index_files = Vec::with_capacity(ix_names.len());
        for ix_name in ix_names {
            let ix = self.indexes.remove(&ix_name).expect("name collected above");
            index_files.push(ix.store.file);
        }
        Ok(DroppedTable {
            heap_file: table.heap.file,
            file_no: table.heap.file_no,
            engine: table.engine_kind,
            index_files,
        })
    }

    pub(crate) fn index_metas(&self) -> Vec<IndexMeta> {
        self.indexes
            .values()
            .map(|ix| IndexMeta {
                name: ix.name.clone(),
                table: ix.table.clone(),
                column: ix.column.clone(),
                unique: ix.unique,
                file_no: ix.store.file_no,
            })
            .collect()
    }

    pub(crate) fn indexes_for(&self, table: &str) -> Vec<&IndexEntry> {
        self.indexes.values().filter(|ix| ix.table == table).collect()
    }

    /// Constraint-backed (unique) indexes on a table.
    pub(crate) fn unique_indexes_for(&self, table: &str) -> Vec<&IndexEntry> {
        self.indexes
            .values()
            .filter(|ix| ix.table == table && ix.unique)
            .collect()
    }

    pub(crate) fn index(&self, name: &str) -> Option<&IndexEntry> {
        self.indexes.get(name)
    }

    /// (heap file_no, FileId) for every table, for WAL replay mapping.
    pub(crate) fn heap_files(&self) -> Vec<(u32, FileId)> {
        self.tables.values().map(|t| (t.heap.file_no, t.heap.file)).collect()
    }

    /// The engine kind and handle for the table owning `file_no`, used to
    /// route WAL replay by storage type.
    pub(crate) fn storage_for_file_no(
        &self,
        file_no: u32,
    ) -> Option<(EngineKind, Arc<dyn TableStorage>)> {
        self.tables
            .values()
            .find(|t| t.heap.file_no == file_no)
            .map(|t| (t.engine_kind, t.engine()))
    }

    pub(crate) fn create_view(&mut self, name: &str, sql: String) -> Result<()> {
        if self.tables.contains_key(name) || self.views.contains_key(name) {
            return Err(Error::Runtime(format!("already exists: {name}")));
        }
        self.views.insert(name.to_string(), sql);
        Ok(())
    }

    pub(crate) fn drop_view(&mut self, name: &str) -> Result<()> {
        self.views
            .remove(name)
            .ok_or_else(|| Error::Runtime(format!("no such view: {name}")))?;
        Ok(())
    }

    /// The stored select text of a view, if the name is one.
    pub fn view(&self, name: &str) -> Option<&String> {
        self.views.get(name)
    }

    pub(crate) fn view_metas(&self) -> Vec<ViewMeta> {
        self.views
            .iter()
            .map(|(name, sql)| ViewMeta { name: name.clone(), sql: sql.clone() })
            .collect()
    }
}
