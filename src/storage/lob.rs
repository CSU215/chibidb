//! Out-of-line large objects.
//!
//! A large value is stored as its own file under a directory, named by a
//! monotonic id, so a row can hold a small reference instead of the bytes.
//! Reads can stream through [`LobReader`] in fixed-size chunks rather than
//! materializing the whole object.
//!
//! Layout: `<dir>/<id>.lob` holds the raw bytes; the file length is the object
//! length. Ids are never reused, even after deletion.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Error, Result};

/// Identifies one stored large object.
pub type LobId = u64;

/// Chunk size used by [`LobReader::next_chunk`].
pub const LOB_CHUNK: usize = 64 * 1024;

const SUFFIX: &str = ".lob";

/// A directory of large objects.
pub struct LobStore {
    dir: PathBuf,
    next_id: AtomicU64,
}

impl LobStore {
    /// Opens (or creates) the store, resuming the id counter after the
    /// largest file already present.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Runtime(format!("cannot create {}: {e}", dir.display())))?;
        let mut max_id = 0u64;
        let entries = std::fs::read_dir(dir)
            .map_err(|e| Error::Runtime(format!("cannot read {}: {e}", dir.display())))?;
        for entry in entries.flatten() {
            if let Some(id) = parse_id(&entry.file_name().to_string_lossy()) {
                max_id = max_id.max(id);
            }
        }
        Ok(Self { dir: dir.to_path_buf(), next_id: AtomicU64::new(max_id + 1) })
    }

    /// Stores `data` as a new object and returns its id.
    pub fn write(&self, data: &[u8]) -> Result<LobId> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let path = self.path(id);
        let mut file = File::create(&path)
            .map_err(|e| Error::Runtime(format!("cannot create {}: {e}", path.display())))?;
        file.write_all(data)
            .map_err(|e| Error::Runtime(format!("cannot write lob: {e}")))?;
        file.sync_all()
            .map_err(|e| Error::Runtime(format!("cannot sync lob: {e}")))?;
        Ok(id)
    }

    /// Materializes the whole object.
    pub fn read(&self, id: LobId) -> Result<Vec<u8>> {
        let path = self.path(id);
        std::fs::read(&path).map_err(|e| Error::Runtime(format!("cannot read lob {id}: {e}")))
    }

    /// Length of the object in bytes.
    pub fn len(&self, id: LobId) -> Result<u64> {
        let path = self.path(id);
        let meta = std::fs::metadata(&path)
            .map_err(|e| Error::Runtime(format!("cannot stat lob {id}: {e}")))?;
        Ok(meta.len())
    }

    pub fn is_empty(&self, id: LobId) -> Result<bool> {
        Ok(self.len(id)? == 0)
    }

    /// A streaming reader over the object.
    pub fn reader(&self, id: LobId) -> Result<LobReader> {
        LobReader::open(self.path(id))
    }

    /// Deletes the object; missing objects are not an error.
    pub fn delete(&self, id: LobId) -> Result<()> {
        let path = self.path(id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Runtime(format!("cannot delete lob {id}: {e}"))),
        }
    }

    fn path(&self, id: LobId) -> PathBuf {
        self.dir.join(format!("{id}{SUFFIX}"))
    }
}

/// A forward-only reader that yields the object in chunks.
pub struct LobReader {
    file: File,
    remaining: u64,
}

impl LobReader {
    fn open(path: PathBuf) -> Result<Self> {
        let file = File::open(&path)
            .map_err(|e| Error::Runtime(format!("cannot open {}: {e}", path.display())))?;
        let remaining = file
            .metadata()
            .map_err(|e| Error::Runtime(format!("cannot stat {}: {e}", path.display())))?
            .len();
        Ok(Self { file, remaining })
    }

    /// Reads up to `buf.len()` bytes; returns 0 at end of object.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(self.remaining) as usize;
        let read = self
            .file
            .read(&mut buf[..want])
            .map_err(|e| Error::Runtime(format!("cannot read lob: {e}")))?;
        self.remaining -= read as u64;
        Ok(read)
    }

    /// The next chunk (at most [`LOB_CHUNK`] bytes), or `None` at EOF.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let want = (LOB_CHUNK as u64).min(self.remaining) as usize;
        let mut buf = vec![0u8; want];
        let read = self
            .file
            .read(&mut buf)
            .map_err(|e| Error::Runtime(format!("cannot read lob: {e}")))?;
        self.remaining -= read as u64;
        buf.truncate(read);
        Ok(Some(buf))
    }

    /// Bytes not yet read.
    pub fn remaining(&self) -> u64 {
        self.remaining
    }
}

fn parse_id(name: &str) -> Option<u64> {
    name.strip_suffix(SUFFIX)?.parse().ok()
}
