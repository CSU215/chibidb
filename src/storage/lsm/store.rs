//! An in-memory LSM store: one mutable memtable on top of immutable SSTables.
//!
//! A flush turns the memtable into a new SSTable; compaction merges every
//! SSTable into one. Values carry a one-byte tag so tombstones survive a flush
//! and keep shadowing older tables until compaction drops them.

use std::collections::BTreeMap;

use crate::storage::lsm::block::DEFAULT_RESTART_INTERVAL;
use crate::storage::lsm::memtable::{MemEntry, MemTable};
use crate::storage::lsm::sstable::{SSTable, SSTableBuilder, SstableScanner};
use crate::Result;

const TAG_TOMBSTONE: u8 = 0;
const TAG_VALUE: u8 = 1;

/// Newest table last, oldest first.
pub struct LsmStore {
    memtable: MemTable,
    sstables: Vec<SSTable>,
    block_size: usize,
}

impl Default for LsmStore {
    fn default() -> Self {
        Self::new(4096)
    }
}

impl LsmStore {
    pub fn new(block_size: usize) -> Self {
        Self { memtable: MemTable::new(), sstables: Vec::new(), block_size }
    }

    pub fn put(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) {
        self.memtable.put(key, value);
    }

    pub fn delete(&mut self, key: impl Into<Vec<u8>>) {
        self.memtable.delete(key);
    }

    /// Newest-wins lookup across the memtable and every SSTable.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(entry) = self.memtable.get(key) {
            return Ok(entry.value().map(<[u8]>::to_vec));
        }
        for sstable in self.sstables.iter().rev() {
            // skip tables whose key range cannot contain the key
            if !sstable.may_contain(key) {
                continue;
            }
            if let Some(encoded) = sstable.get(key)? {
                return Ok(decode_entry(&encoded).value().map(<[u8]>::to_vec));
            }
        }
        Ok(None)
    }

    /// The merged, visible contents in ascending key order.
    pub fn iter(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut merged: BTreeMap<Vec<u8>, MemEntry> = BTreeMap::new();
        for sstable in &self.sstables {
            for (key, encoded) in sstable.iter()? {
                merged.insert(key, decode_entry(&encoded));
            }
        }
        for (key, entry) in self.memtable.iter() {
            merged.insert(key, entry);
        }
        Ok(merged
            .into_iter()
            .filter_map(|(key, entry)| entry.value().map(|v| (key, v.to_vec())))
            .collect())
    }

    pub fn num_sstables(&self) -> usize {
        self.sstables.len()
    }

    /// A cheap snapshot for streaming scans: the memtable entries and clones
    /// of the SSTables (their contents are shared behind `Arc`).
    pub fn snapshot(&self) -> (Vec<(Vec<u8>, MemEntry)>, Vec<SSTable>) {
        (self.memtable.iter(), self.sstables.clone())
    }

    /// The memtable as an SSTable image, or `None` when it is empty. Does not
    /// mutate the store, so a caller can persist the image first.
    pub fn memtable_image(&self) -> Option<Vec<u8>> {
        if self.memtable.is_empty() {
            return None;
        }
        Some(self.build_from(self.memtable.iter()))
    }

    /// The major-compaction image of all SSTables (tombstones dropped), or
    /// `None` when there is nothing to merge.
    pub fn compacted_image(&self) -> Result<Option<Vec<u8>>> {
        if self.sstables.len() < 2 {
            return Ok(None);
        }
        let mut merged: BTreeMap<Vec<u8>, MemEntry> = BTreeMap::new();
        for sstable in &self.sstables {
            for (key, encoded) in sstable.iter()? {
                merged.insert(key, decode_entry(&encoded));
            }
        }
        Ok(Some(self.build_from(merged.into_iter().collect::<Vec<_>>())))
    }

    pub fn add_sstable(&mut self, sstable: SSTable) {
        self.sstables.push(sstable);
    }

    pub fn replace_sstables(&mut self, sstables: Vec<SSTable>) {
        self.sstables = sstables;
    }

    pub fn reset_memtable(&mut self) {
        self.memtable = MemTable::new();
    }

    /// Turns the memtable into a new SSTable (minor compaction).
    pub fn flush(&mut self) -> Result<()> {
        if let Some(image) = self.memtable_image() {
            self.add_sstable(SSTable::parse(image)?);
            self.reset_memtable();
        }
        Ok(())
    }

    /// Merges every SSTable into one, dropping shadowed values and tombstones
    /// (major compaction).
    pub fn compact(&mut self) -> Result<()> {
        if let Some(image) = self.compacted_image()? {
            self.replace_sstables(vec![SSTable::parse(image)?]);
        }
        Ok(())
    }

    fn build_from(&self, entries: Vec<(Vec<u8>, MemEntry)>) -> Vec<u8> {
        let mut builder = SSTableBuilder::new(self.block_size, DEFAULT_RESTART_INTERVAL);
        for (key, entry) in entries {
            builder.add(&key, &encode_entry(&entry));
        }
        builder.finish()
    }
}

fn encode_entry(entry: &MemEntry) -> Vec<u8> {
    match entry {
        MemEntry::Tombstone => vec![TAG_TOMBSTONE],
        MemEntry::Value(value) => {
            let mut buf = Vec::with_capacity(value.len() + 1);
            buf.push(TAG_VALUE);
            buf.extend_from_slice(value);
            buf
        }
    }
}

fn decode_entry(data: &[u8]) -> MemEntry {
    match data.first() {
        Some(&TAG_VALUE) => MemEntry::Value(data[1..].to_vec()),
        _ => MemEntry::Tombstone,
    }
}

/// Streams the newest visible value per key across a memtable and a list of
/// SSTables, without materializing the whole store. Sources are ordered newest
/// first; on a key collision the newest source wins, and a winning tombstone
/// suppresses the key.
pub struct MergeScanner {
    sources: Vec<MergeSource>,
    heads: Vec<Option<(Vec<u8>, MemEntry)>>,
}

enum MergeSource {
    Mem(std::vec::IntoIter<(Vec<u8>, MemEntry)>),
    Sst(SstableScanner),
}

impl MergeScanner {
    /// `sstables` must be newest first.
    pub fn new(mem: Vec<(Vec<u8>, MemEntry)>, sstables: Vec<SSTable>) -> Result<Self> {
        let mut sources = vec![MergeSource::Mem(mem.into_iter())];
        for table in sstables {
            sources.push(MergeSource::Sst(table.scanner()));
        }
        let mut heads = Vec::with_capacity(sources.len());
        for source in &mut sources {
            heads.push(advance_one(source)?);
        }
        Ok(Self { sources, heads })
    }

    pub fn next_entry(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        loop {
            let mut best: Option<Vec<u8>> = None;
            for head in &self.heads {
                if let Some((key, _)) = head
                    && best.as_ref().is_none_or(|current| key < current)
                {
                    best = Some(key.clone());
                }
            }
            let Some(key) = best else {
                return Ok(None);
            };

            let mut winner: Option<MemEntry> = None;
            for i in 0..self.sources.len() {
                let matches = matches!(&self.heads[i], Some((k, _)) if k.as_slice() == key.as_slice());
                if matches {
                    let (_, entry) = self.heads[i].take().expect("head matched");
                    if winner.is_none() {
                        winner = Some(entry);
                    }
                    self.heads[i] = advance_one(&mut self.sources[i])?;
                }
            }

            match winner.expect("at least one source matched") {
                MemEntry::Value(value) => return Ok(Some((key, value))),
                MemEntry::Tombstone => continue,
            }
        }
    }
}

fn advance_one(source: &mut MergeSource) -> Result<Option<(Vec<u8>, MemEntry)>> {
    match source {
        MergeSource::Mem(iter) => Ok(iter.next()),
        MergeSource::Sst(scanner) => Ok(scanner
            .next_entry()?
            .map(|(key, encoded)| (key, decode_entry(&encoded)))),
    }
}
