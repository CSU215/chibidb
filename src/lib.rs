//! chaoticdb — a small, hand-written relational database in pure Rust.

pub mod catalog;
pub mod config;
pub mod db;
pub mod error;
pub mod exec;
pub mod index;
pub mod instance;
pub mod net;
pub mod sql;
pub mod storage;
pub mod txn;
pub mod value;
pub mod wal;

pub use db::Database;
pub use error::{Error, Result};
pub use sql::datetime;
pub use sql::result::ResultSet;
pub use txn::trx::Session;
pub use value::{DataType, Value};

pub use net::{client, http, mysql, protocol, render, server, wire};
pub use net::run_repl;

#[allow(unused_imports)]
pub(crate) use db::{conflict_error, serialization_error};
