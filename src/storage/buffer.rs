use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::storage::disk::DiskManager;
use crate::storage::page::{zeroed_page, FileId, PageData, PageNo, PAGE_SIZE};
use crate::{Error, Result};

/// One cached page. Its key is fixed for the frame's lifetime, so a reader
/// holding an `Arc` keeps reading the page it loaded even if the frame is
/// later evicted. `data` is the page latch; `dirty` is set by writers.
struct Frame {
    file: FileId,
    no: PageNo,
    data: Mutex<PageData>,
    dirty: AtomicBool,
}

/// The page table and LRU order, guarded by one lock. Frame *contents* have
/// their own latches, so this lock is held only while resolving a page to a
/// frame, never while a closure runs.
#[derive(Default)]
struct PoolState {
    frames: HashMap<(FileId, PageNo), Arc<Frame>>,
    lru: VecDeque<(FileId, PageNo)>,
}

/// A snapshot of the buffer pool's lookup counters, for observability and
/// cache-behavior tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Lookups served from a resident frame.
    pub hits: u64,
    /// Lookups that had to load a page from disk.
    pub misses: u64,
    /// Frames written back to make room for a miss.
    pub evictions: u64,
}

/// Thread-safe buffer pool: `&self` methods let readers share the pool while
/// per-frame latches keep different pages independent.
pub struct BufferPool {
    disk: Mutex<DiskManager>,
    state: Mutex<PoolState>,
    capacity: usize,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl BufferPool {
    pub fn new(disk: DiskManager, capacity: usize) -> Self {
        Self {
            disk: Mutex::new(disk),
            state: Mutex::new(PoolState::default()),
            capacity: capacity.max(1),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// A consistent-enough snapshot of the lookup counters.
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
        }
    }

    pub fn with_page<T>(
        &self,
        file: FileId,
        no: PageNo,
        f: impl FnOnce(&mut [u8; PAGE_SIZE]) -> Result<T>,
    ) -> Result<T> {
        let frame = self.frame_for(file, no)?;
        let mut data = frame.data.lock();
        let out = f(&mut data);
        frame.dirty.store(true, Ordering::Release);
        out
    }

    pub fn read_page<T>(
        &self,
        file: FileId,
        no: PageNo,
        f: impl FnOnce(&[u8; PAGE_SIZE]) -> Result<T>,
    ) -> Result<T> {
        let frame = self.frame_for(file, no)?;
        let data = frame.data.lock();
        f(&data)
    }

    /// Resolves a page to its frame, loading and evicting as needed. The state
    /// lock is released before the caller latches the frame, so a page latch is
    /// never held together with the metadata lock.
    fn frame_for(&self, file: FileId, no: PageNo) -> Result<Arc<Frame>> {
        let key = (file, no);
        let mut state = self.state.lock();
        if let Some(frame) = state.frames.get(&key).cloned() {
            touch(&mut state.lru, key);
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(frame);
        }
        while state.frames.len() >= self.capacity {
            let victim_key = state
                .lru
                .pop_front()
                .ok_or_else(|| Error::Runtime("buffer pool exhausted".into()))?;
            self.evictions.fetch_add(1, Ordering::Relaxed);
            let victim = state
                .frames
                .remove(&victim_key)
                .expect("lru and page table stay in sync");
            if victim.dirty.load(Ordering::Acquire) {
                // lock order disk -> frame latch, matching flush_all
                let mut disk = self.disk.lock();
                let data = victim.data.lock();
                disk.write_page(victim.file, victim.no, &data)?;
                victim.dirty.store(false, Ordering::Release);
            }
        }
        let mut data = zeroed_page();
        self.disk.lock().read_page(file, no, &mut data)?;
        self.misses.fetch_add(1, Ordering::Relaxed);
        let frame = Arc::new(Frame {
            file,
            no,
            data: Mutex::new(data),
            dirty: AtomicBool::new(false),
        });
        state.frames.insert(key, frame.clone());
        state.lru.push_back(key);
        Ok(frame)
    }

    pub fn alloc_page(&self, file: FileId) -> Result<PageNo> {
        let mut disk = self.disk.lock();
        let no = disk.page_count(file)?;
        let empty = zeroed_page();
        disk.write_page(file, no, &empty)?;
        Ok(no)
    }

    pub fn create_file(&self, path: &Path) -> Result<FileId> {
        self.disk.lock().create_file(path)
    }

    pub fn open_file(&self, path: &Path) -> Result<FileId> {
        self.disk.lock().open_file(path)
    }

    pub fn page_count(&self, file: FileId) -> Result<PageNo> {
        self.disk.lock().page_count(file)
    }

    /// Empties a file in place. Cached frames of the file must be dropped
    /// first (see `discard_file`).
    pub fn truncate_file(&self, file: FileId) -> Result<()> {
        self.disk.lock().truncate_file(file)
    }

    /// Drops all cached frames of a file without writing them back, closes
    /// its handle and returns the path for deletion.
    pub fn close_file(&self, file: FileId) -> Result<PathBuf> {
        self.discard_file(file);
        self.disk.lock().close_file(file)
    }

    /// Drops all cached frames of a file without writing them back. A reader
    /// that already resolved a frame keeps its own `Arc`, so it is unaffected.
    pub fn discard_file(&self, file: FileId) {
        let mut state = self.state.lock();
        state.frames.retain(|k, _| k.0 != file);
        state.lru.retain(|k| k.0 != file);
    }

    pub fn flush_file(&self, file: FileId) -> Result<()> {
        let frames = self.dirty_frames(Some(file));
        if frames.is_empty() {
            return Ok(());
        }
        let mut disk = self.disk.lock();
        for frame in frames {
            if frame.dirty.load(Ordering::Acquire) {
                let data = frame.data.lock();
                disk.write_page(frame.file, frame.no, &data)?;
                frame.dirty.store(false, Ordering::Release);
            }
        }
        Ok(())
    }

    pub fn flush_all(&self) -> Result<()> {
        let frames = self.dirty_frames(None);
        if frames.is_empty() {
            return Ok(());
        }
        let mut disk = self.disk.lock();
        // double-write: stage every page and sync before touching the final
        // files, so a crash mid-write can be repaired on the next open
        for frame in &frames {
            let data = frame.data.lock();
            disk.stage_page(frame.file, frame.no, &data)?;
        }
        disk.sync_double_write()?;
        for frame in &frames {
            let data = frame.data.lock();
            disk.write_page(frame.file, frame.no, &data)?;
            frame.dirty.store(false, Ordering::Release);
        }
        disk.reset_double_write()?;
        Ok(())
    }

    /// Snapshots the frames that need writing back, optionally only those of
    /// one file. `flush_*` then writes them without holding the state lock.
    fn dirty_frames(&self, only: Option<FileId>) -> Vec<Arc<Frame>> {
        let state = self.state.lock();
        state
            .frames
            .iter()
            .filter(|(k, f)| {
                only.is_none_or(|file| k.0 == file) && f.dirty.load(Ordering::Acquire)
            })
            .map(|(_, f)| f.clone())
            .collect()
    }
}

/// Moves `key` to the back of the LRU deque.
fn touch(lru: &mut VecDeque<(FileId, PageNo)>, key: (FileId, PageNo)) {
    if let Some(pos) = lru.iter().position(|&k| k == key) {
        lru.remove(pos);
    }
    lru.push_back(key);
}

impl Drop for BufferPool {
    fn drop(&mut self) {
        let _ = self.flush_all();
    }
}
