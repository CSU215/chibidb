//! chaoticdb — a small, hand-written relational database in pure Rust.
//!
//! The crate is layered, top to bottom:
//!
//! ```text
//! sql/     lexer → ast → parser, shared value/result types
//! exec/    planning and Volcano operators over the storage seams
//! catalog/ table, index and view metadata
//! txn/     snapshots, commit status, row locks, SSI
//! storage/ buffer pool, disk, slotted pages, heap, codec, LSM
//! index/   order-preserving keys and the B+ tree
//! wal.rs   write-ahead log and crash recovery
//! db/      Database: ties catalog + storage + txn + wal together
//! instance.rs multi-database routing, users and privileges
//! net/     REPL, text/HTTP/MySQL front-ends
//! ```
//!
//! Data flows one way for reads (SQL → parse → plan → operators → storage) and
//! through the transaction layer for writes (operators → mvcc store ops → WAL).

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

pub use error::{Error, Result};
pub use sql::datetime;
pub use sql::result::ResultSet;
pub use value::{DataType, Value};
