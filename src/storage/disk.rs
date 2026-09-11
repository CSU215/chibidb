use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::storage::dwb::DoubleWrite;
use crate::storage::page::{FileId, PageNo, PAGE_SIZE};
use crate::{Error, Result};

pub struct DiskManager {
    files: BTreeMap<FileId, (PathBuf, File)>,
    next_file_id: FileId,
    dwb: Option<DoubleWrite>,
}

impl Default for DiskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DiskManager {
    pub fn new() -> Self {
        Self { files: BTreeMap::new(), next_file_id: 0, dwb: None }
    }

    /// Enables a double-write buffer: dirty pages are staged there and synced
    /// before reaching their final location, so a torn final write can be
    /// repaired on the next open.
    pub fn enable_double_write(&mut self, path: &Path) -> Result<()> {
        self.dwb = Some(DoubleWrite::open(path)?);
        Ok(())
    }

    /// Stages a page into the double-write buffer (no-op when disabled).
    pub fn stage_page(&mut self, file: FileId, no: PageNo, buf: &[u8; PAGE_SIZE]) -> Result<()> {
        if let Some(dwb) = self.dwb.as_mut() {
            let target = self
                .files
                .get(&file)
                .map(|(p, _)| p.clone())
                .ok_or_else(|| Error::Runtime(format!("unknown file id {file}")))?;
            dwb.stage(&target, no, buf)?;
        }
        Ok(())
    }

    pub fn sync_double_write(&mut self) -> Result<()> {
        if let Some(dwb) = self.dwb.as_mut() {
            dwb.sync()?;
        }
        Ok(())
    }

    pub fn reset_double_write(&mut self) -> Result<()> {
        if let Some(dwb) = self.dwb.as_mut() {
            dwb.reset()?;
        }
        Ok(())
    }

    pub fn create_file(&mut self, path: &Path) -> Result<FileId> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| Error::Runtime(format!("cannot create file {}: {e}", path.display())))?;
        self.register(path, file)
    }

    pub fn open_file(&mut self, path: &Path) -> Result<FileId> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| Error::Runtime(format!("cannot open file {}: {e}", path.display())))?;
        self.register(path, file)
    }

    fn register(&mut self, path: &Path, file: File) -> Result<FileId> {
        let id = self.next_file_id;
        self.next_file_id += 1;
        self.files.insert(id, (path.to_path_buf(), file));
        Ok(id)
    }

    /// Closes the handle of a file that is about to be deleted and returns
    /// its path (Windows cannot delete a file while a handle is open).
    pub fn close_file(&mut self, file: FileId) -> Result<PathBuf> {
        let (path, _) = self
            .files
            .remove(&file)
            .ok_or_else(|| Error::Runtime(format!("unknown file id {file}")))?;
        Ok(path)
    }

    pub fn read_page(&mut self, file: FileId, no: PageNo, buf: &mut [u8; PAGE_SIZE]) -> Result<()> {
        let f = self.file(file)?;
        let offset = no as u64 * PAGE_SIZE as u64;
        let len = f.metadata().map_err(io_err)?.len();
        buf.fill(0);
        if offset >= len {
            return Ok(());
        }
        f.seek(SeekFrom::Start(offset)).map_err(io_err)?;
        f.read_exact(buf).map_err(io_err)?;
        Ok(())
    }

    pub fn write_page(&mut self, file: FileId, no: PageNo, buf: &[u8; PAGE_SIZE]) -> Result<()> {
        let f = self.file(file)?;
        let offset = no as u64 * PAGE_SIZE as u64;
        let len = f.metadata().map_err(io_err)?.len();
        if offset > len {
            // fill the gap with zeros so the file stays page-aligned
            f.seek(SeekFrom::Start(len)).map_err(io_err)?;
            let gap = [0u8; PAGE_SIZE];
            let mut pos = len;
            while pos < offset {
                let n = PAGE_SIZE.min((offset - pos) as usize);
                f.write_all(&gap[..n]).map_err(io_err)?;
                pos += n as u64;
            }
        }
        f.seek(SeekFrom::Start(offset)).map_err(io_err)?;
        f.write_all(buf).map_err(io_err)?;
        f.flush().map_err(io_err)
    }

    pub fn page_count(&mut self, file: FileId) -> Result<PageNo> {
        let f = self.file(file)?;
        let len = f.metadata().map_err(io_err)?.len();
        Ok((len / PAGE_SIZE as u64) as PageNo)
    }

    /// Empties a file in place (used when rebuilding derived structures).
    pub fn truncate_file(&mut self, file: FileId) -> Result<()> {
        let f = self.file(file)?;
        f.set_len(0).map_err(io_err)
    }

    fn file(&mut self, file: FileId) -> Result<&mut File> {
        self.files
            .get_mut(&file)
            .ok_or_else(|| Error::Runtime(format!("unknown file id {file}")))
            .map(|(_, f)| f)
    }
}

fn io_err(e: std::io::Error) -> Error {
    Error::Runtime(format!("io error: {e}"))
}
