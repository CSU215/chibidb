use std::collections::{HashMap, VecDeque};

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

    pub fn page_count(&mut self, file: FileId) -> Result<PageNo> {
        self.disk.page_count(file)
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
