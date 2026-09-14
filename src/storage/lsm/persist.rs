//! On-disk LSM store: SSTable files plus a manifest describing the levels.
//!
//! Directory layout:
//! ```text
//! <dir>/MANIFEST          levels of live SSTable file numbers + next id
//! <dir>/sst-000001.sst    one immutable table per file
//! ```
//! Flush and each compaction step write a new table file before the manifest
//! is rewritten (write to a temp file, fsync, rename), so a crash leaves the
//! old manifest or the new one. Orphaned files are ignored on open.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::storage::lsm::memtable::MemEntry;
use crate::storage::lsm::sstable::SSTable;
use crate::storage::lsm::store::LsmStore;
use crate::{Error, Result};

pub use crate::storage::lsm::store::DEFAULT_COMPACTION_TRIGGER;

const MANIFEST: &str = "MANIFEST";
const MANIFEST_TMP: &str = "MANIFEST.tmp";
const MANIFEST_MAGIC: [u8; 8] = *b"LSMMF002";
const SSTABLE_SUFFIX: &str = ".sst";

/// A durable LSM store rooted at one directory.
pub struct PersistentLsm {
    dir: PathBuf,
    store: LsmStore,
    next_file_no: u32,
}

impl PersistentLsm {
    /// Opens (or creates) the store, restoring the levels named by the
    /// manifest. Any extra files on disk are ignored.
    pub fn open(dir: &Path, block_size: usize) -> Result<Self> {
        Self::open_with_trigger(dir, block_size, DEFAULT_COMPACTION_TRIGGER)
    }

    /// Like [`PersistentLsm::open`] but with an explicit compaction trigger.
    pub fn open_with_trigger(
        dir: &Path,
        block_size: usize,
        compaction_trigger: usize,
    ) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Runtime(format!("cannot create {}: {e}", dir.display())))?;
        let (stored_next, manifest_levels) = read_manifest(dir)?;
        let mut store = LsmStore::new_with_trigger(block_size, compaction_trigger);
        let mut max_file = 0u32;
        let mut levels = Vec::with_capacity(manifest_levels.len());
        for level in &manifest_levels {
            let mut tables = Vec::with_capacity(level.len());
            for &no in level {
                let bytes = std::fs::read(sstable_path(dir, no))
                    .map_err(|e| Error::Runtime(format!("cannot read sstable {no}: {e}")))?;
                tables.push(SSTable::parse(bytes)?.with_file_no(no));
                max_file = max_file.max(no);
            }
            levels.push(tables);
        }
        store.set_levels(levels);
        let next_file_no = stored_next.max(max_file + 1).max(1);
        Ok(Self { dir: dir.to_path_buf(), store, next_file_no })
    }

    pub fn put(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) {
        self.store.put(key, value);
    }

    pub fn delete(&mut self, key: impl Into<Vec<u8>>) {
        self.store.delete(key);
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.store.get(key)
    }

    pub fn iter(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.store.iter()
    }

    pub fn num_sstables(&self) -> usize {
        self.store.num_sstables()
    }

    pub fn memtable_bytes(&self) -> usize {
        self.store.memtable_bytes()
    }

    pub fn level_counts(&self) -> Vec<usize> {
        self.store.level_counts()
    }

    /// File numbers of every live table, newest first.
    pub fn sstable_file_numbers(&self) -> Vec<u32> {
        self.store
            .levels()
            .iter()
            .flatten()
            .filter_map(SSTable::file_no)
            .collect()
    }

    /// A cheap snapshot for streaming scans (memtable entries + table clones).
    pub fn snapshot(&self) -> (Vec<(Vec<u8>, MemEntry)>, Vec<SSTable>) {
        self.store.snapshot()
    }

    /// Writes the memtable to a new level-0 table, cascades compaction, then
    /// commits the new level layout via the manifest.
    pub fn flush(&mut self) -> Result<()> {
        let Some(image) = self.store.memtable_image() else {
            return Ok(());
        };
        let no = self.next_file_no;
        write_sstable(&self.dir, no, &image)?;
        self.next_file_no += 1;
        self.store.insert_level0(SSTable::parse(image)?.with_file_no(no));
        self.store.reset_memtable();
        let mut obsolete = Vec::new();
        self.cascade(&mut obsolete)?;
        // Commit the new table layout before deleting the files it replaced:
        // otherwise a crash in between leaves the durable manifest pointing at
        // removed files.
        self.write_manifest()?;
        for file in obsolete {
            let _ = std::fs::remove_file(sstable_path(&self.dir, file));
        }
        Ok(())
    }

    /// Merges every level into one table, dropping tombstones (major
    /// compaction).
    pub fn compact(&mut self) -> Result<()> {
        let Some(image) = self.store.compacted_image()? else {
            return Ok(());
        };
        let old = self.sstable_file_numbers();
        let no = self.next_file_no;
        write_sstable(&self.dir, no, &image)?;
        self.next_file_no += 1;
        self.store.set_levels(vec![vec![SSTable::parse(image)?.with_file_no(no)]]);
        // Commit the manifest before removing the obsolete tables (see flush).
        self.write_manifest()?;
        for file in old {
            let _ = std::fs::remove_file(sstable_path(&self.dir, file));
        }
        Ok(())
    }

    /// Merges every level that reached the trigger into the next level. The
    /// replaced file numbers are collected in `obsolete` for the caller to
    /// delete *after* the manifest is committed.
    fn cascade(&mut self, obsolete: &mut Vec<u32>) -> Result<()> {
        while let Some(level) = self.store.level_needing_compaction() {
            let tables: Vec<SSTable> = self.store.level_tables(level).to_vec();
            let image = self.store.merge_tables(&tables)?;
            let no = self.next_file_no;
            write_sstable(&self.dir, no, &image)?;
            self.next_file_no += 1;
            let merged = SSTable::parse(image)?.with_file_no(no);
            for table in &tables {
                if let Some(file) = table.file_no() {
                    obsolete.push(file);
                }
            }
            self.store.apply_merge(level, merged);
        }
        Ok(())
    }

    fn write_manifest(&self) -> Result<()> {
        let levels: Vec<Vec<u32>> = self
            .store
            .levels()
            .iter()
            .map(|level| level.iter().filter_map(SSTable::file_no).collect())
            .collect();
        write_manifest(&self.dir, self.next_file_no, &levels)
    }
}

fn sstable_path(dir: &Path, file_no: u32) -> PathBuf {
    dir.join(format!("sst-{file_no:06}{SSTABLE_SUFFIX}"))
}

fn write_sstable(dir: &Path, file_no: u32, image: &[u8]) -> Result<()> {
    let path = sstable_path(dir, file_no);
    let mut file = File::create(&path)
        .map_err(|e| Error::Runtime(format!("cannot create {}: {e}", path.display())))?;
    file.write_all(image).map_err(|e| Error::Runtime(format!("cannot write sstable: {e}")))?;
    file.sync_all().map_err(|e| Error::Runtime(format!("cannot sync sstable: {e}")))
}

fn write_manifest(dir: &Path, next_file_no: u32, levels: &[Vec<u32>]) -> Result<()> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&MANIFEST_MAGIC);
    buf.extend_from_slice(&next_file_no.to_le_bytes());
    buf.extend_from_slice(&(levels.len() as u32).to_le_bytes());
    for level in levels {
        buf.extend_from_slice(&(level.len() as u32).to_le_bytes());
        for &no in level {
            buf.extend_from_slice(&no.to_le_bytes());
        }
    }
    let tmp = dir.join(MANIFEST_TMP);
    {
        let mut file = File::create(&tmp)
            .map_err(|e| Error::Runtime(format!("cannot create manifest: {e}")))?;
        file.write_all(&buf).map_err(|e| Error::Runtime(format!("cannot write manifest: {e}")))?;
        file.sync_all().map_err(|e| Error::Runtime(format!("cannot sync manifest: {e}")))?;
    }
    std::fs::rename(&tmp, dir.join(MANIFEST))
        .map_err(|e| Error::Runtime(format!("cannot replace manifest: {e}")))
}

fn read_manifest(dir: &Path) -> Result<(u32, Vec<Vec<u32>>)> {
    let path = dir.join(MANIFEST);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((1, Vec::new())),
        Err(e) => return Err(Error::Runtime(format!("cannot read manifest: {e}"))),
    };
    if bytes.len() < 16 || bytes[0..8] != MANIFEST_MAGIC {
        return Err(Error::Runtime("manifest is corrupt".into()));
    }
    let next_file_no = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let num_levels = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let mut pos = 16;
    let mut levels = Vec::with_capacity(num_levels);
    for _ in 0..num_levels {
        let count = take_u32(&bytes, &mut pos)? as usize;
        if bytes.len() < pos + count * 4 {
            return Err(Error::Runtime("manifest is truncated".into()));
        }
        let mut level = Vec::with_capacity(count);
        for _ in 0..count {
            level.push(take_u32(&bytes, &mut pos)?);
        }
        levels.push(level);
    }
    Ok((next_file_no, levels))
}

fn take_u32(data: &[u8], pos: &mut usize) -> Result<u32> {
    let bytes = data
        .get(*pos..*pos + 4)
        .ok_or_else(|| Error::Runtime("manifest is truncated".into()))?;
    *pos += 4;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}
