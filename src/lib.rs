pub mod ast;
mod error;
pub mod lexer;
pub mod parser;
mod repl;

pub use error::{Error, Result};
pub use repl::run_repl;

pub struct Database;

impl Database {
    pub fn open_in_memory() -> Self {
        Self
    }

    pub fn execute_sql(&mut self, sql: &str) -> Result<()> {
        if sql.trim().is_empty() {
            return Ok(());
        }
        Err(Error::Unsupported)
    }
}
