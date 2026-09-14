//! An in-memory LSM store: one mutable memtable on top of immutable SSTables
//! organized into levels.
//!
//! A flush appends a table to level 0. When a level reaches the compaction
//! trigger it is merged into a single table placed at the front of the next
//! level (a logarithmic, binary-counter structure). Because newer levels are
//! always placed first, scanning levels in order preserves newest-wins while
//! bounding write amplification to O(N log N). Values carry a one-byte tag so
//! tombstones survive merges and keep shadowing older levels.

use std::collections::BTreeMap;

use crate::storage::lsm::block::DEFAULT_RESTART_INTERVAL;
use crate::storage::lsm::memtable::{MemEntry, MemTable};
use crate::storage::lsm::sstable::{SSTable, SSTableBuilder, SstableScanner};
use crate::Result;

const TAG_TOMBSTONE: u8 = 0;
const TAG_VALUE: u8 = 1;

/// Default tables per level that triggers a merge into the next level.
pub const DEFAULT_COMPACTION_TRIGGER: usize = 4;

/// Levels of immutable tables. `levels[0]` is newest; within a level the front
/// is newest. Flattening in order yields a globally newest-first sequence.
pub struct LsmStore {
    memtable: MemTable,
    levels: Vec<Vec<SSTable>>,
    block_size: usize,
    compaction_trigger: usize,
}

impl Default for LsmStore {
    fn default() -> Self {
        Self::new(4096)
    }
}

impl LsmStore {
    pub fn new(block_size: usize) -> Self {
        Self::new_with_trigger(block_size, DEFAULT_COMPACTION_TRIGGER)
    }

    pub fn new_with_trigger(block_size: usize, compaction_trigger: usize) -> Self {
        Self {
            memtable: MemTable::new(),
            levels: Vec::new(),
            block_size,
            compaction_trigger: compaction_trigger.max(2),
        }
    }

    pub fn put(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) {
        self.memtable.put(key, value);
    }

    pub fn delete(&mut self, key: impl Into<Vec<u8>>) {
        self.memtable.delete(key);
    }

    /// Newest-wins lookup across the memtable and every level.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(entry) = self.memtable.get(key) {
            return Ok(entry.value().map(<[u8]>::to_vec));
        }
        for level in &self.levels {
            for table in level {
                // skip tables whose key range cannot contain the key
                if !table.may_contain(key) {
                    continue;
                }
                if let Some(encoded) = table.get(key)? {
                    return Ok(decode_entry(&encoded).value().map(<[u8]>::to_vec));
                }
            }
        }
        Ok(None)
    }

    /// Tables newest first across all levels.
    fn tables_newest_first(&self) -> impl Iterator<Item = &SSTable> {
        self.levels.iter().flatten()
    }

    /// The merged, visible contents in ascending key order.
    pub fn iter(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut merged: BTreeMap<Vec<u8>, MemEntry> = BTreeMap::new();
        for table in self.tables_newest_first() {
            for (key, encoded) in table.iter()? {
                merged.entry(key).or_insert_with(|| decode_entry(&encoded));
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
        self.levels.iter().map(Vec::len).sum()
    }

    pub fn levels(&self) -> &[Vec<SSTable>] {
        &self.levels
    }

    pub fn set_levels(&mut self, levels: Vec<Vec<SSTable>>) {
        self.levels = levels;
    }

    pub fn compaction_trigger(&self) -> usize {
        self.compaction_trigger
    }

    /// A cheap snapshot for streaming scans: the memtable entries and clones
    /// of the SSTables (their contents are shared behind `Arc`), newest first.
    pub fn snapshot(&self) -> (Vec<(Vec<u8>, MemEntry)>, Vec<SSTable>) {
        (self.memtable.iter(), self.tables_newest_first().cloned().collect())
    }

    /// The memtable as an SSTable image, or `None` when it is empty.
    pub fn memtable_image(&self) -> Option<Vec<u8>> {
        if self.memtable.is_empty() {
            return None;
        }
        Some(self.build_from(self.memtable.iter()))
    }

    /// The major-compaction image of every table (tombstones dropped), or
    /// `None` when there is nothing to merge.
    pub fn compacted_image(&self) -> Result<Option<Vec<u8>>> {
        if self.num_sstables() < 2 {
            return Ok(None);
        }
        let mut merged: BTreeMap<Vec<u8>, MemEntry> = BTreeMap::new();
        for table in self.tables_newest_first() {
            for (key, encoded) in table.iter()? {
                merged.entry(key).or_insert_with(|| decode_entry(&encoded));
            }
        }
        let entries: Vec<(Vec<u8>, MemEntry)> = merged
            .into_iter()
            .filter(|(_, entry)| entry.value().is_some())
            .collect();
        Ok(Some(self.build_from(entries)))
    }

    pub fn reset_memtable(&mut self) {
        self.memtable = MemTable::new();
    }

    /// Inserts a freshly flushed table at the front of level 0.
    pub fn insert_level0(&mut self, table: SSTable) {
        if self.levels.is_empty() {
            self.levels.push(Vec::new());
        }
        self.levels[0].insert(0, table);
    }

    /// The lowest level holding at least `compaction_trigger` tables.
    pub fn level_needing_compaction(&self) -> Option<usize> {
        self.levels.iter().position(|level| level.len() >= self.compaction_trigger)
    }

    pub fn level_tables(&self, level: usize) -> &[SSTable] {
        &self.levels[level]
    }

    /// Merges `tables` (newest first) into one image, keeping tombstones so
    /// they keep shadowing older levels.
    pub fn merge_tables(&self, tables: &[SSTable]) -> Result<Vec<u8>> {
        let mut merged: BTreeMap<Vec<u8>, MemEntry> = BTreeMap::new();
        for table in tables {
            for (key, encoded) in table.iter()? {
                merged.entry(key).or_insert_with(|| decode_entry(&encoded));
            }
        }
        Ok(self.build_from(merged.into_iter().collect()))
    }

    /// Replaces `level` with nothing and puts `merged` at the front of the
    /// next level.
    pub fn apply_merge(&mut self, level: usize, merged: SSTable) {
        self.levels[level].clear();
        if self.levels.len() <= level + 1 {
            self.levels.resize(level + 2, Vec::new());
        }
        self.levels[level + 1].insert(0, merged);
    }

    /// Turns the memtable into a new level-0 table (minor compaction) and
    /// cascades any level that reached the trigger.
    pub fn flush(&mut self) -> Result<()> {
        if let Some(image) = self.memtable_image() {
            self.insert_level0(SSTable::parse(image)?);
            self.reset_memtable();
            self.cascade()?;
        }
        Ok(())
    }

    fn cascade(&mut self) -> Result<()> {
        while let Some(level) = self.level_needing_compaction() {
            let tables: Vec<SSTable> = self.level_tables(level).to_vec();
            let merged = SSTable::parse(self.merge_tables(&tables)?)?;
            self.apply_merge(level, merged);
        }
        Ok(())
    }

    /// Merges every table into one, dropping shadowed values and tombstones.
    pub fn compact(&mut self) -> Result<()> {
        if let Some(image) = self.compacted_image()? {
            self.levels = vec![vec![SSTable::parse(image)?]];
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
