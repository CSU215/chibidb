use std::collections::{HashMap, VecDeque};
use std::path::Path;

use crate::storage::disk::DiskManager;
use crate::storage::page::{zeroed_page, FileId, PageData, PageNo, PAGE_SIZE};
use crate::{Error, Result};

struct Frame {
    key: (FileId, PageNo),
    data: PageData,
    dirty: bool,
}

pub struct BufferPool {
    disk: DiskManager,
    frames: Vec<Frame>,
    page_table: HashMap<(FileId, PageNo), usize>,
    lru: VecDeque<usize>,
    capacity: usize,
}

impl BufferPool {
    pub fn new(disk: DiskManager, capacity: usize) -> Self {
        Self {
            disk,
            frames: Vec::new(),
            page_table: HashMap::new(),
            lru: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn with_page<T>(
        &mut self,
        file: FileId,
        no: PageNo,
        f: impl FnOnce(&mut [u8; PAGE_SIZE]) -> Result<T>,
    ) -> Result<T> {
        let idx = self.frame_for(file, no)?;
        let out = f(&mut self.frames[idx].data);
        self.frames[idx].dirty = true;
        out
    }

    pub fn read_page<T>(
        &mut self,
        file: FileId,
        no: PageNo,
        f: impl FnOnce(&[u8; PAGE_SIZE]) -> Result<T>,
    ) -> Result<T> {
        let idx = self.frame_for(file, no)?;
        f(&self.frames[idx].data)
    }

    fn frame_for(&mut self, file: FileId, no: PageNo) -> Result<usize> {
        let key = (file, no);
        if let Some(&idx) = self.page_table.get(&key) {
            self.touch(idx);
            return Ok(idx);
        }
        let idx = self.victim()?;
        let frame = &mut self.frames[idx];
        self.disk.read_page(file, no, &mut frame.data)?;
        frame.key = key;
        frame.dirty = false;
        self.page_table.insert(key, idx);
        self.touch(idx);
        Ok(idx)
    }

    fn victim(&mut self) -> Result<usize> {
        if self.frames.len() < self.capacity {
            self.frames.push(Frame { key: (0, 0), data: zeroed_page(), dirty: false });
            return Ok(self.frames.len() - 1);
        }
        let idx =
            self.lru.pop_front().ok_or_else(|| Error::Runtime("buffer pool exhausted".into()))?;
        let frame = &mut self.frames[idx];
        if frame.dirty {
            let (file, no) = frame.key;
            self.disk.write_page(file, no, &frame.data)?;
        }
        self.page_table.remove(&frame.key);
        Ok(idx)
    }

    fn touch(&mut self, idx: usize) {
        if let Some(pos) = self.lru.iter().position(|&x| x == idx) {
            self.lru.remove(pos);
        }
        self.lru.push_back(idx);
    }

    pub fn alloc_page(&mut self, file: FileId) -> Result<PageNo> {
        let no = self.disk.page_count(file)?;
        let empty = zeroed_page();
        self.disk.write_page(file, no, &empty)?;
        Ok(no)
    }

    pub fn create_file(&mut self, path: &Path) -> Result<FileId> {
        self.disk.create_file(path)
    }

    pub fn open_file(&mut self, path: &Path) -> Result<FileId> {
        self.disk.open_file(path)
    }

    pub fn page_count(&mut self, file: FileId) -> Result<PageNo> {
        self.disk.page_count(file)
    }

    /// Empties a file in place. Cached frames of the file must be dropped
    /// first (see `discard_file`).
    pub fn truncate_file(&mut self, file: FileId) -> Result<()> {
        self.disk.truncate_file(file)
    }

    /// Drops all cached frames of a file without writing them back, closes
    /// its handle and returns the path for deletion.
    pub fn close_file(&mut self, file: FileId) -> Result<std::path::PathBuf> {
        self.discard_file(file);
        self.disk.close_file(file)
    }

    /// Drops all cached frames of a file without writing them back.
    pub fn discard_file(&mut self, file: FileId) {
        // every live frame is referenced exactly once by the lru list
        let lru_order: Vec<usize> = self.lru.drain(..).collect();
        let mut old_frames: Vec<Option<Frame>> =
            std::mem::take(&mut self.frames).into_iter().map(Some).collect();
        let mut remap: HashMap<usize, usize> = HashMap::new();
        let mut new_frames = Vec::with_capacity(old_frames.len());
        for old_idx in lru_order {
            if let Some(frame) = old_frames[old_idx].take() {
                if frame.key.0 != file {
                    remap.insert(old_idx, new_frames.len());
                    new_frames.push(frame);
                }
            }
        }
        self.page_table.retain(|k, _| k.0 != file);
        for v in self.page_table.values_mut() {
            *v = remap[v];
        }
        self.frames = new_frames;
        self.lru = (0..self.frames.len()).collect();
    }

    pub fn flush_file(&mut self, file: FileId) -> Result<()> {
        for i in 0..self.frames.len() {
            if self.frames[i].dirty && self.frames[i].key.0 == file {
                let (f, no) = self.frames[i].key;
                self.disk.write_page(f, no, &self.frames[i].data)?;
                self.frames[i].dirty = false;
            }
        }
        Ok(())
    }

    pub fn flush_all(&mut self) -> Result<()> {
        for i in 0..self.frames.len() {
            if self.frames[i].dirty {
                let (file, no) = self.frames[i].key;
                self.disk.write_page(file, no, &self.frames[i].data)?;
                self.frames[i].dirty = false;
            }
        }
        Ok(())
    }
}

impl Drop for BufferPool {
    fn drop(&mut self) {
        let _ = self.flush_all();
    }
}
