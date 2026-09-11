//! On-disk LSM store: SSTable files plus a manifest listing the live set.
//!
//! Directory layout:
//! ```text
//! <dir>/MANIFEST          number-encoded list of live SSTable files
//! <dir>/sst-000001.sst    one immutable table per file
//! ```
//! Flush writes a new table file and then rewrites the manifest (write to a
//! temp file, fsync, rename) before adopting it, so a crash leaves either the
//! old manifest or the new one. An orphaned table file is ignored on open
//! because the manifest never named it.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::storage::lsm::sstable::SSTable;
use crate::storage::lsm::store::LsmStore;
use crate::{Error, Result};

const MANIFEST: &str = "MANIFEST";
const MANIFEST_TMP: &str = "MANIFEST.tmp";
const MANIFEST_MAGIC: [u8; 8] = *b"LSMMF001";
const SSTABLE_SUFFIX: &str = ".sst";

/// A durable LSM store rooted at one directory.
pub struct PersistentLsm {
    dir: PathBuf,
    store: LsmStore,
    /// Live table file numbers, oldest first.
    sstable_files: Vec<u32>,
    next_file_no: u32,
}

impl PersistentLsm {
    /// Opens (or creates) the store, restoring the tables named by the
    /// manifest. Any extra files on disk are ignored.
    pub fn open(dir: &Path, block_size: usize) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Runtime(format!("cannot create {}: {e}", dir.display())))?;
        let sstable_files = read_manifest(dir)?;
        let mut store = LsmStore::new(block_size);
        for &no in &sstable_files {
            let bytes = std::fs::read(sstable_path(dir, no))
                .map_err(|e| Error::Runtime(format!("cannot read sstable {no}: {e}")))?;
            store.add_sstable(SSTable::parse(bytes)?);
        }
        let next_file_no = sstable_files.iter().copied().max().unwrap_or(0) + 1;
        Ok(Self { dir: dir.to_path_buf(), store, sstable_files, next_file_no })
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

    pub fn sstable_file_numbers(&self) -> &[u32] {
        &self.sstable_files
    }

    /// Writes the memtable out as one new SSTable and commits it via the
    /// manifest.
    pub fn flush(&mut self) -> Result<()> {
        let Some(image) = self.store.memtable_image() else {
            return Ok(());
        };
        let no = self.next_file_no;
        write_sstable(&self.dir, no, &image)?;
        self.sstable_files.push(no);
        if let Err(e) = write_manifest(&self.dir, &self.sstable_files) {
            // roll back the in-memory manifest so disk and memory agree
            self.sstable_files.pop();
            return Err(e);
        }
        self.next_file_no += 1;
        self.store.add_sstable(SSTable::parse(image)?);
        self.store.reset_memtable();
        Ok(())
    }

    /// Merges every SSTable into one new file and removes the old ones.
    pub fn compact(&mut self) -> Result<()> {
        let Some(image) = self.store.compacted_image()? else {
            return Ok(());
        };
        let old = std::mem::take(&mut self.sstable_files);
        let no = self.next_file_no;
        write_sstable(&self.dir, no, &image)?;
        if let Err(e) = write_manifest(&self.dir, &[no]) {
            self.sstable_files = old;
            return Err(e);
        }
        self.next_file_no += 1;
        self.store.replace_sstables(vec![SSTable::parse(image)?]);
        self.sstable_files = vec![no];
        for file in old {
            let _ = std::fs::remove_file(sstable_path(&self.dir, file));
        }
        Ok(())
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

fn write_manifest(dir: &Path, file_numbers: &[u32]) -> Result<()> {
    let mut buf = Vec::with_capacity(12 + file_numbers.len() * 4);
    buf.extend_from_slice(&MANIFEST_MAGIC);
    buf.extend_from_slice(&(file_numbers.len() as u32).to_le_bytes());
    for &no in file_numbers {
        buf.extend_from_slice(&no.to_le_bytes());
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

fn read_manifest(dir: &Path) -> Result<Vec<u32>> {
    let path = dir.join(MANIFEST);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Runtime(format!("cannot read manifest: {e}"))),
    };
    if bytes.len() < 12 || bytes[0..8] != MANIFEST_MAGIC {
        return Err(Error::Runtime("manifest is corrupt".into()));
    }
    let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    if bytes.len() < 12 + count * 4 {
        return Err(Error::Runtime("manifest is truncated".into()));
    }
    let mut files = Vec::with_capacity(count);
    let mut pos = 12;
    for _ in 0..count {
        files.push(u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()));
        pos += 4;
    }
    Ok(files)
}
