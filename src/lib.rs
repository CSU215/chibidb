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

use crate::catalog::Catalog;

pub struct Database {
    catalog: Catalog,
}

impl Database {
    pub fn open_in_memory() -> Self {
        Self { catalog: Catalog::default() }
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
}
