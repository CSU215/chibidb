use std::collections::BTreeMap;

use crate::ast::DataType;
use crate::storage::FileId;
use crate::{Error, Result};

pub mod meta;

use meta::TableMeta;

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDesc {
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
}

#[derive(Debug)]
pub(crate) struct HeapStore {
    pub file: FileId,
    pub file_no: u32,
}

#[derive(Debug)]
pub struct Table {
    pub schema: Schema,
    pub(crate) heap: HeapStore,
}

#[derive(Debug, Default)]
pub struct Catalog {
    tables: BTreeMap<String, Table>,
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
}
