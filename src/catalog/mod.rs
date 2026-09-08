use std::collections::BTreeMap;

use crate::ast::DataType;
use crate::storage::FileId;
use crate::value::Value;
use crate::{Error, Result};

pub mod meta;

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
pub(crate) enum RowStore {
    Mem(Vec<Vec<Value>>),
    Heap { file: FileId },
}

#[derive(Debug)]
pub struct Table {
    pub schema: Schema,
    pub(crate) store: RowStore,
}

#[derive(Debug, Default)]
pub struct Catalog {
    tables: BTreeMap<String, Table>,
}

impl Catalog {
    pub(crate) fn create_table(&mut self, name: &str, schema: Schema, store: RowStore) -> Result<()> {
        if self.tables.contains_key(name) {
            return Err(Error::Runtime(format!("table already exists: {name}")));
        }
        self.tables.insert(name.to_string(), Table { schema, store });
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
}
