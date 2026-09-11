//! Sorted string tables: immutable files of data blocks plus an index.
//!
//! Layout:
//! ```text
//! [data blocks][bloom filter][index block][filter off/size][index off/size][magic 8]
//! ```
//! The index block maps the last key of each data block to that block's
//! `(offset, size)`. A reader consults the bloom filter first, then
//! binary-searches the index and reads the chosen block.

use std::sync::Arc;

use crate::storage::lsm::bloom::{BloomBuilder, BloomFilter};
use crate::storage::lsm::block::{Block, BlockBuilder, DEFAULT_RESTART_INTERVAL};
use crate::storage::lsm::coding::{get_varint64, put_varint64};
use crate::{Error, Result};

const MAGIC: [u8; 8] = *b"SSTBL002";
const FOOTER_LEN: usize = 8 * 4 + 8;
const DEFAULT_BLOCK_SIZE: usize = 4096;
const BLOOM_BITS_PER_KEY: usize = 10;

/// Byte range of one data block within the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHandle {
    pub offset: u64,
    pub size: u64,
}

impl BlockHandle {
    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        put_varint64(&mut buf, self.offset);
        put_varint64(&mut buf, self.size);
        buf
    }

    fn decode(data: &[u8]) -> Result<Self> {
        let mut pos = 0;
        let offset = get_varint64(data, &mut pos)?;
        let size = get_varint64(data, &mut pos)?;
        Ok(Self { offset, size })
    }
}

/// Streams sorted key/value pairs into one SSTable image.
pub struct SSTableBuilder {
    block_size: usize,
    data: Vec<u8>,
    index_block: BlockBuilder,
    current: BlockBuilder,
    bloom: BloomBuilder,
    last_key: Vec<u8>,
    started: bool,
    num_entries: usize,
}

impl Default for SSTableBuilder {
    fn default() -> Self {
        Self::new(DEFAULT_BLOCK_SIZE, DEFAULT_RESTART_INTERVAL)
    }
}

impl SSTableBuilder {
    pub fn new(block_size: usize, restart_interval: usize) -> Self {
        Self {
            block_size: block_size.max(64),
            data: Vec::new(),
            index_block: BlockBuilder::new(restart_interval),
            current: BlockBuilder::new(restart_interval),
            bloom: BloomBuilder::new(BLOOM_BITS_PER_KEY),
            last_key: Vec::new(),
            started: false,
            num_entries: 0,
        }
    }

    /// Adds a pair; keys must arrive in strictly ascending order.
    pub fn add(&mut self, key: &[u8], value: &[u8]) {
        debug_assert!(
            !self.started || self.last_key.as_slice() < key,
            "sstable keys must be added in ascending order"
        );
        self.started = true;
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.current.add(key, value);
        self.bloom.add(key);
        self.num_entries += 1;
        if self.current.current_size_estimate() >= self.block_size {
            self.flush_block();
        }
    }

    pub fn num_entries(&self) -> usize {
        self.num_entries
    }

    /// Flushes the pending data block, writes the index and footer, and
    /// returns the complete table image.
    pub fn finish(&mut self) -> Vec<u8> {
        self.flush_block();
        let filter = self.bloom.finish();
        let filter_handle = BlockHandle {
            offset: self.data.len() as u64,
            size: filter.len() as u64,
        };
        self.data.extend_from_slice(&filter);
        let index_block = self.index_block.finish();
        let index_handle = BlockHandle {
            offset: self.data.len() as u64,
            size: index_block.len() as u64,
        };
        self.data.extend_from_slice(&index_block);
        for handle in [filter_handle, index_handle] {
            self.data.extend_from_slice(&handle.offset.to_le_bytes());
            self.data.extend_from_slice(&handle.size.to_le_bytes());
        }
        self.data.extend_from_slice(&MAGIC);
        std::mem::take(&mut self.data)
    }

    fn flush_block(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let block = self.current.finish();
        let handle = BlockHandle {
            offset: self.data.len() as u64,
            size: block.len() as u64,
        };
        self.data.extend_from_slice(&block);
        self.index_block.add(&self.last_key, &handle.encode());
    }
}

/// A parsed, immutable SSTable held in memory.
#[derive(Clone)]
pub struct SSTable {
    data: Arc<Vec<u8>>,
    /// (last key of a data block, where the block lives), ascending.
    index: Arc<Vec<(Vec<u8>, BlockHandle)>>,
    bloom: Arc<BloomFilter>,
    /// Smallest and largest keys, for range pruning.
    first_key: Arc<Vec<u8>>,
    last_key: Arc<Vec<u8>>,
}

impl SSTable {
    /// Parses a table image produced by [`SSTableBuilder`].
    pub fn parse(data: Vec<u8>) -> Result<Self> {
        if data.len() < FOOTER_LEN {
            return Err(Error::Runtime("sstable is too short".into()));
        }
        let magic_at = data.len() - 8;
        if data[magic_at..] != MAGIC {
            return Err(Error::Runtime("sstable magic mismatch".into()));
        }
        let mut footer = data.len() - FOOTER_LEN;
        let filter_offset = u64::from_le_bytes(data[footer..footer + 8].try_into().unwrap());
        footer += 8;
        let filter_size = u64::from_le_bytes(data[footer..footer + 8].try_into().unwrap());
        footer += 8;
        let index_offset = u64::from_le_bytes(data[footer..footer + 8].try_into().unwrap());
        footer += 8;
        let index_size = u64::from_le_bytes(data[footer..footer + 8].try_into().unwrap());
        let filter_handle = BlockHandle { offset: filter_offset, size: filter_size };
        let index_handle = BlockHandle { offset: index_offset, size: index_size };

        let bloom = BloomFilter::decode(slice(&data, filter_handle)?.to_vec());
        let index_block = Block::parse(slice(&data, index_handle)?.to_vec())?;
        let mut index = Vec::new();
        for (key, encoded) in index_block.entries()? {
            index.push((key, BlockHandle::decode(&encoded)?));
        }
        let first_key = match index.first() {
            Some((_, handle)) => {
                Block::parse(slice(&data, *handle)?.to_vec())?.first_key()?.unwrap_or_default()
            }
            None => Vec::new(),
        };
        let last_key = index.last().map(|(key, _)| key.clone()).unwrap_or_default();
        Ok(Self {
            data: Arc::new(data),
            index: Arc::new(index),
            bloom: Arc::new(bloom),
            first_key: Arc::new(first_key),
            last_key: Arc::new(last_key),
        })
    }

    pub fn num_blocks(&self) -> usize {
        self.index.len()
    }

    /// The first key in the table, if any.
    pub fn first_key(&self) -> Result<Option<Vec<u8>>> {
        Ok((!self.first_key.is_empty()).then(|| (*self.first_key).clone()))
    }

    /// The last key in the table, if any.
    pub fn last_key(&self) -> Option<Vec<u8>> {
        (!self.last_key.is_empty()).then(|| (*self.last_key).clone())
    }

    /// Whether `key` could be in this table, by key range. Empty tables never
    /// contain anything.
    pub fn may_contain(&self, key: &[u8]) -> bool {
        !self.index.is_empty()
            && key >= self.first_key.as_slice()
            && key <= self.last_key.as_slice()
    }

    /// The table's bloom filter, exposed for tests and diagnostics.
    pub fn bloom(&self) -> &BloomFilter {
        &self.bloom
    }

    /// Exact lookup.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if !self.bloom.maybe_contains(key) {
            return Ok(None);
        }
        let pos = self.index.partition_point(|(last, _)| last.as_slice() < key);
        if pos >= self.index.len() {
            return Ok(None);
        }
        self.block(self.index[pos].1)?.get(key)
    }

    /// All entries in ascending order.
    pub fn iter(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        for (_, handle) in self.index.iter() {
            out.extend(self.block(*handle)?.entries()?);
        }
        Ok(out)
    }

    fn block(&self, handle: BlockHandle) -> Result<Block> {
        Block::parse(slice(&self.data, handle)?.to_vec())
    }

    /// A lazy scanner over the table, holding one data block at a time.
    pub fn scanner(&self) -> SstableScanner {
        SstableScanner { table: self.clone(), next_block: 0, current: Vec::new().into_iter() }
    }
}

/// Lazily walks an SSTable's data blocks, materializing only the current one.
pub struct SstableScanner {
    table: SSTable,
    next_block: usize,
    current: std::vec::IntoIter<(Vec<u8>, Vec<u8>)>,
}

impl SstableScanner {
    pub fn next_entry(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        loop {
            if let Some(entry) = self.current.next() {
                return Ok(Some(entry));
            }
            if self.next_block >= self.table.index.len() {
                return Ok(None);
            }
            let handle = self.table.index[self.next_block].1;
            self.next_block += 1;
            self.current = self.table.block(handle)?.entries()?.into_iter();
        }
    }
}

fn slice(data: &[u8], handle: BlockHandle) -> Result<&[u8]> {
    let start = handle.offset as usize;
    let end = start
        .checked_add(handle.size as usize)
        .ok_or_else(|| Error::Runtime("sstable block handle overflows".into()))?;
    data.get(start..end)
        .ok_or_else(|| Error::Runtime("sstable block handle out of range".into()))
}
