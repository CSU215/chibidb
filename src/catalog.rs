use std::collections::BTreeMap;

use crate::ast::DataType;
use crate::value::Value;
use crate::{Error, Result};

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

#[derive(Debug, Default)]
pub struct Table {
    pub schema: Schema,
    rows: Vec<Vec<Value>>,
}

impl Table {
    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }

    pub fn push_row(&mut self, row: Vec<Value>) {
        self.rows.push(row);
    }
}

#[derive(Debug, Default)]
pub struct Catalog {
    tables: BTreeMap<String, Table>,
}

impl Catalog {
    pub fn create_table(&mut self, name: &str, schema: Schema) -> Result<()> {
        if self.tables.contains_key(name) {
            return Err(Error::Runtime(format!("table already exists: {name}")));
        }
        self.tables
            .insert(name.to_string(), Table { schema, rows: Vec::new() });
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

    pub fn insert_row(&mut self, name: &str, row: Vec<Value>) -> Result<()> {
        self.table_mut(name)?.push_row(row);
        Ok(())
    }
}
