pub mod ast;
mod error;
pub mod exec;
pub mod lexer;
pub mod parser;
mod repl;
pub mod result;
pub mod value;

pub use error::{Error, Result};
pub use repl::run_repl;
pub use result::ResultSet;

pub struct Database;

impl Database {
    pub fn open_in_memory() -> Self {
        Self
    }

    pub fn execute_sql(&mut self, sql: &str) -> Result<Vec<ResultSet>> {
        let stmts = parser::parse(sql)?;
        stmts.iter().map(exec::execute).collect()
    }
}
