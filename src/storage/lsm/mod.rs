//! LSM-tree building blocks. The write buffer (`MemTable`) and the sorted
//! block format land first; SSTables, compaction and the table engine follow.

pub mod block;
pub mod coding;
pub mod memtable;

pub use block::{Block, BlockBuilder, DEFAULT_RESTART_INTERVAL};
pub use memtable::{MemEntry, MemTable};
