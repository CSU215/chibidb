use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use parking_lot::{Mutex, MutexGuard};

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
    /// 活跃的 pin 数。非零期间这一帧不会被选为淘汰对象，所以闭包里的写入
    /// 不可能落进一个已脱离页表的孤儿帧。见 `PinnedFrame`。
    pins: AtomicU32,
}

/// RAII pin：存活期间该帧不会被淘汰。构造**只在持有 `state` 锁时**发生，
/// 因此"帧进入页表"与"帧被 pin"之间不存在窗口；`Drop` 只碰帧自己的原子量，
/// 不重新获取 `state`（解 pin 只会让帧变得可淘汰，晚一点可见是保守的）。
struct PinnedFrame {
    frame: Arc<Frame>,
}

impl PinnedFrame {
    /// 把 `frame` 的 pin 计数加一。调用者必须持有 `state` 锁，否则就只是把
    /// 原来的竞态窗口挪了个位置。
    fn new(frame: Arc<Frame>) -> Self {
        frame.pins.fetch_add(1, Ordering::Acquire);
        Self { frame }
    }

    fn file(&self) -> FileId {
        self.frame.file
    }

    fn no(&self) -> PageNo {
        self.frame.no
    }

    /// 页闩。pin 保证帧还在页表里，页闩保证内容不被并发改写。
    fn data(&self) -> MutexGuard<'_, PageData> {
        self.frame.data.lock()
    }

    fn is_dirty(&self) -> bool {
        self.frame.dirty.load(Ordering::Acquire)
    }

    fn mark_dirty(&self) {
        self.frame.dirty.store(true, Ordering::Release);
    }

    fn clear_dirty(&self) {
        self.frame.dirty.store(false, Ordering::Release);
    }
}

impl Drop for PinnedFrame {
    fn drop(&mut self) {
        let prev = self.frame.pins.fetch_sub(1, Ordering::Release);
        debug_assert!(prev > 0, "unbalanced pin on ({}, {})", self.frame.file, self.frame.no);
    }
}

/// The page table and LRU order, guarded by one lock. Frame *contents* have
/// their own latches, so this lock is held only while resolving a page to a
/// frame, never while a closure runs.
///
/// 不变式 Ⅳ：每个存活帧在 `lru` 里恰好登记一次（`lru.len() == frames.len()`）。
/// pin 不移除登记，只是让该帧失去淘汰候选资格 —— 所以被 pin 的帧仍在队列里。
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
        let pinned = self.frame_for(file, no)?;
        let mut data = pinned.data();
        let out = f(&mut data);
        pinned.mark_dirty();
        out
    }

    pub fn read_page<T>(
        &self,
        file: FileId,
        no: PageNo,
        f: impl FnOnce(&[u8; PAGE_SIZE]) -> Result<T>,
    ) -> Result<T> {
        let pinned = self.frame_for(file, no)?;
        let data = pinned.data();
        f(&data)
    }

    /// Resolves a page to its frame, loading and evicting as needed. The state
    /// lock is released before the caller latches the frame, so a page latch is
    /// never held together with the metadata lock. The returned guard holds a
    /// pin, so the frame cannot be evicted while the caller still uses it.
    fn frame_for(&self, file: FileId, no: PageNo) -> Result<PinnedFrame> {
        let key = (file, no);
        let mut state = self.state.lock();
        // `frames` 可变借用于淘汰，`lru` 可变借用于重排；拆开字段让两者并存。
        let PoolState { frames, lru } = &mut *state;
        if let Some(frame) = frames.get(&key).cloned() {
            touch(lru, key);
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(PinnedFrame::new(frame));
        }
        while frames.len() >= self.capacity {
            // 被 pin 的帧留在 LRU 队列里，只是不参与淘汰 —— 否则它们会丢掉
            // 淘汰顺序信息（不变式 Ⅳ：登记数恒等于页表长度）。
            let victim_key = take_victim(lru, frames).ok_or_else(|| {
                Error::Runtime("buffer pool exhausted: every frame is pinned".into())
            })?;
            self.evictions.fetch_add(1, Ordering::Relaxed);
            let victim =
                frames.remove(&victim_key).expect("lru and page table stay in sync");
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
            pins: AtomicU32::new(0),
        });
        frames.insert(key, frame.clone());
        lru.push_back(key);
        debug_assert_eq!(frames.len(), lru.len(), "invariant Ⅳ: lru mirrors the page table");
        Ok(PinnedFrame::new(frame))
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
    ///
    /// 被 pin 的帧**不**被跳过：这是"这份数据已经不要了"的单写者契约，调用者
    /// （`rebuild_indexes` / `drop_table`）持库级写锁，因此正常情况下没有在飞的
    /// 闭包。真有的话，它的修改随最后一个 `Arc` 一起消失，这正是本方法要的语义。
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
            if frame.is_dirty() {
                let data = frame.data();
                disk.write_page(frame.file(), frame.no(), &data)?;
                frame.clear_dirty();
            }
        }
        disk.sync_file(file)?;
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
            let data = frame.data();
            disk.stage_page(frame.file(), frame.no(), &data)?;
        }
        disk.sync_double_write()?;
        for frame in &frames {
            let data = frame.data();
            disk.write_page(frame.file(), frame.no(), &data)?;
            frame.clear_dirty();
        }
        // The final pages must be durable before the DWB can be discarded;
        // otherwise a crash after reset would lose them with no repair copy.
        let mut files: Vec<FileId> = frames.iter().map(|f| f.file()).collect();
        files.sort_unstable();
        files.dedup();
        for file in files {
            disk.sync_file(file)?;
        }
        disk.reset_double_write()?;
        Ok(())
    }

    /// Snapshots the frames that need writing back, optionally only those of
    /// one file. `flush_*` then writes them without holding the state lock.
    /// The snapshot pins every frame it returns: otherwise a concurrent
    /// eviction could write the same page (or drop it) under our feet.
    fn dirty_frames(&self, only: Option<FileId>) -> Vec<PinnedFrame> {
        let state = self.state.lock();
        state
            .frames
            .iter()
            .filter(|(k, f)| {
                only.is_none_or(|file| k.0 == file) && f.dirty.load(Ordering::Acquire)
            })
            .map(|(_, f)| PinnedFrame::new(f.clone()))
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

/// Picks the least recently used *evictable* frame, removing its registration.
/// Frames with a live pin stay in the deque so they keep their place in the
/// order; returning `None` means every registered frame is pinned.
fn take_victim(
    lru: &mut VecDeque<(FileId, PageNo)>,
    frames: &HashMap<(FileId, PageNo), Arc<Frame>>,
) -> Option<(FileId, PageNo)> {
    let pos = lru
        .iter()
        .position(|k| frames.get(k).is_none_or(|f| f.pins.load(Ordering::Acquire) == 0))?;
    lru.remove(pos)
}

impl Drop for BufferPool {
    fn drop(&mut self) {
        let _ = self.flush_all();
    }
}
