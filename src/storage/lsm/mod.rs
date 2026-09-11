//! LSM-tree building blocks. The write buffer (`MemTable`) lands first;
//! SSTables, compaction and the table engine follow.

pub mod memtable;

pub use memtable::{MemEntry, MemTable};
