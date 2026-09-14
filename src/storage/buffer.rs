use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use parking_lot::{Mutex, MutexGuard};

use crate::config::{EvictionPolicy, ObservabilityConfig};
use crate::storage::disk::DiskManager;
use crate::storage::page::{zeroed_page, FileId, PageData, PageNo, PAGE_SIZE};
use crate::storage::replacer::{from_policy, FrameVitals, Key, Replacer};
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
    /// CLOCK 的引用位：命中或刚载入时置位，被淘汰器扫到时清零（"第二次机会"）。
    /// 其它策略不读它。
    accessed: AtomicBool,
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

/// The page table and the eviction order, guarded by one lock. Frame
/// *contents* have their own latches, so this lock is held only while
/// resolving a page to a frame, never while a closure runs.
///
/// 不变式 Ⅳ：每个存活帧在淘汰器里恰好登记一次（`replacer.len() == frames.len()`）。
/// pin 不移除登记，只是让该帧失去淘汰候选资格 —— 所以被 pin 的帧仍在队列/环里。
struct PoolState {
    frames: HashMap<Key, Arc<Frame>>,
    replacer: Box<dyn Replacer>,
}

/// 把池的页表暴露给淘汰器。pin 数与引用位**只**存在帧上，策略不保存副本。
struct PoolVitals<'a> {
    frames: &'a HashMap<Key, Arc<Frame>>,
}

impl FrameVitals for PoolVitals<'_> {
    fn pins(&self, key: Key) -> u32 {
        self.frames.get(&key).map_or(u32::MAX, |f| f.pins.load(Ordering::Acquire))
    }

    fn referenced(&self, key: Key) -> bool {
        self.frames.get(&key).is_some_and(|f| f.accessed.load(Ordering::Relaxed))
    }

    fn clear_referenced(&self, key: Key) {
        if let Some(frame) = self.frames.get(&key) {
            frame.accessed.store(false, Ordering::Relaxed);
        }
    }
}

/// A snapshot of the buffer pool's lookup counters, for observability and
/// cache-behavior tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Lookups served from a resident frame.
    pub hits: u64,
    /// Lookups that had to load a page from disk.
    pub misses: u64,
    /// Frames chosen for replacement to make room for a miss.
    pub evictions: u64,
    /// Evictions of a dirty frame, i.e. those that triggered a write-back.
    pub dirty_evictions: u64,
    /// Frames currently resident in the pool.
    pub resident: u64,
    /// Configured frame capacity.
    pub capacity: u64,
}

impl PoolStats {
    /// Fraction of lookups served from cache, in `0.0..=1.0`; `0.0` with no
    /// lookups.
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 { 0.0 } else { self.hits as f64 / total as f64 }
    }

    /// Evictions whose frame was clean, so no write-back was needed.
    pub fn clean_evictions(&self) -> u64 {
        self.evictions - self.dirty_evictions
    }
}

/// Sink for buffer-pool observability events. The pool only calls it when the
/// matching `[observability]` switch is on, so a reporter can assume it is
/// wanted. Kept as a trait so tests can capture events instead of stderr.
pub trait CacheReporter: Send + Sync {
    /// One frame was replaced. `dirty` means it was written back.
    fn evict(&self, key: Key, dirty: bool, stats: &PoolStats);
    /// A snapshot requested at a checkpoint or on shutdown.
    fn stats(&self, stats: &PoolStats);
}

/// Default reporter: one line per event on stderr. `policy` labels eviction
/// lines, since a pool has exactly one eviction policy.
pub struct StderrReporter {
    policy: EvictionPolicy,
}

impl StderrReporter {
    pub fn new(policy: EvictionPolicy) -> Self {
        Self { policy }
    }
}

impl CacheReporter for StderrReporter {
    fn evict(&self, key: Key, dirty: bool, stats: &PoolStats) {
        eprintln!(
            "chibidb[buffer]: evict (file={},page={}) policy={} dirty={} resident={}/{} hit={:.1}%",
            key.0,
            key.1,
            policy_label(self.policy),
            dirty,
            stats.resident,
            stats.capacity,
            stats.hit_rate() * 100.0,
        );
    }

    fn stats(&self, stats: &PoolStats) {
        eprintln!(
            "chibidb[buffer]: stats hits={} misses={} evictions={} clean={} dirty={} resident={}/{} hit_rate={:.1}%",
            stats.hits,
            stats.misses,
            stats.evictions,
            stats.clean_evictions(),
            stats.dirty_evictions,
            stats.resident,
            stats.capacity,
            stats.hit_rate() * 100.0,
        );
    }
}

fn policy_label(policy: EvictionPolicy) -> &'static str {
    match policy {
        EvictionPolicy::Lru => "lru",
        EvictionPolicy::Clock => "clock",
        EvictionPolicy::Fifo => "fifo",
    }
}

/// Thread-safe buffer pool: `&self` methods let readers share the pool while
/// per-frame latches keep different pages independent.
pub struct BufferPool {
    /// 裸字段：`DiskManager` 自己已经按文件加锁，再套一层 `Mutex` 只会
    /// 把不同文件的 I/O 重新串起来。
    disk: DiskManager,
    state: Mutex<PoolState>,
    /// 串行化整段 DWB 协议（stage → sync → 写回 → sync → reset）。逐文件锁
    /// 把旧全局 `Mutex<DiskManager>` 顺带提供的互斥拆掉了，这里显式补回来，
    /// 否则两个并发的 `flush_all`（如机会式 checkpoint）会交错 stage/reset。
    /// 只加在 checkpoint 路径上，热路径无成本。
    flush_lock: Mutex<()>,
    capacity: usize,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
    dirty_evictions: AtomicU64,
    /// Which observability events to emit; both off by default.
    observability: ObservabilityConfig,
    /// Event sink, called only when the matching switch above is on.
    reporter: Arc<dyn CacheReporter>,
}

impl BufferPool {
    /// 使用默认淘汰策略（`lru`）开池。
    pub fn new(disk: DiskManager, capacity: usize) -> Self {
        Self::new_with_eviction(disk, capacity, EvictionPolicy::Lru)
    }

    pub fn new_with_eviction(
        disk: DiskManager,
        capacity: usize,
        eviction: EvictionPolicy,
    ) -> Self {
        Self::new_with_observability(disk, capacity, eviction, ObservabilityConfig::default())
    }

    /// Like [`BufferPool::new_with_eviction`] but with the observability
    /// switches applied; the stderr reporter is the production sink.
    pub fn new_with_observability(
        disk: DiskManager,
        capacity: usize,
        eviction: EvictionPolicy,
        observability: ObservabilityConfig,
    ) -> Self {
        Self::new_with_reporter(
            disk,
            capacity,
            eviction,
            observability,
            Arc::new(StderrReporter::new(eviction)),
        )
    }

    /// Full constructor: a custom [`CacheReporter`] can capture events in
    /// tests. Events are still gated by `observability`.
    pub fn new_with_reporter(
        disk: DiskManager,
        capacity: usize,
        eviction: EvictionPolicy,
        observability: ObservabilityConfig,
        reporter: Arc<dyn CacheReporter>,
    ) -> Self {
        Self {
            disk,
            state: Mutex::new(PoolState { frames: HashMap::new(), replacer: from_policy(eviction) }),
            flush_lock: Mutex::new(()),
            capacity: capacity.max(1),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            dirty_evictions: AtomicU64::new(0),
            observability,
            reporter,
        }
    }

    /// A consistent-enough snapshot of the lookup counters and residency.
    pub fn stats(&self) -> PoolStats {
        let resident = self.state.lock().frames.len() as u64;
        PoolStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            dirty_evictions: self.dirty_evictions.load(Ordering::Relaxed),
            resident,
            capacity: self.capacity as u64,
        }
    }

    /// Emits a snapshot through the reporter when `observability.cache_stats`
    /// is on. Called at checkpoints and on shutdown.
    pub fn report_stats(&self) {
        if self.observability.cache_stats {
            self.reporter.stats(&self.stats());
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
        // 本轮淘汰的帧，锁外再上报（替换日志不能在持 `state` 锁时做 I/O）。
        let mut evicted: Vec<(Key, bool)> = Vec::new();
        let frame = {
            let mut state = self.state.lock();
            // `frames` 可变借用于淘汰，`replacer` 可变借用于重排；拆开字段让两者并存。
            let PoolState { frames, replacer } = &mut *state;
            if let Some(frame) = frames.get(&key).cloned() {
                replacer.record_access(key);
                frame.accessed.store(true, Ordering::Relaxed); // CLOCK 的引用位
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(PinnedFrame::new(frame));
            }
            while frames.len() >= self.capacity {
                // 被 pin 的帧留在队列/环里，只是不参与淘汰 —— 否则它们会丢掉
                // 淘汰顺序（不变式 Ⅳ：登记数恒等于页表长度）。
                let victim_key = {
                    let vitals = PoolVitals { frames };
                    replacer.choose_victim(&vitals)
                }
                .ok_or_else(|| {
                    Error::Runtime("buffer pool exhausted: every frame is pinned".into())
                })?;
                self.evictions.fetch_add(1, Ordering::Relaxed);
                let victim = frames
                    .remove(&victim_key)
                    .expect("the replacer stays in sync with the page table");
                let dirty = victim.dirty.load(Ordering::Acquire);
                if dirty {
                    // 锁序：页闩 → 文件锁，与 flush_* 同向。被选中的帧 pins == 0，
                    // 而页闩的持有期是 pin 持有期的子集，所以这句不会阻塞（§6.3）。
                    self.dirty_evictions.fetch_add(1, Ordering::Relaxed);
                    let data = victim.data.lock();
                    self.disk.write_page(victim.file, victim.no, &data)?;
                    victim.dirty.store(false, Ordering::Release);
                }
                evicted.push((victim_key, dirty));
            }
            let mut data = zeroed_page();
            self.disk.read_page(file, no, &mut data)?;
            self.misses.fetch_add(1, Ordering::Relaxed);
            let frame = Arc::new(Frame {
                file,
                no,
                data: Mutex::new(data),
                dirty: AtomicBool::new(false),
                pins: AtomicU32::new(0),
                accessed: AtomicBool::new(true), // 刚载入 → 有第二次机会
            });
            frames.insert(key, frame.clone());
            replacer.push(key);
            debug_assert_eq!(frames.len(), replacer.len(), "invariant Ⅳ: the replacer mirrors the page table");
            frame
        };
        if self.observability.eviction_log && !evicted.is_empty() {
            let stats = self.stats();
            for (victim_key, dirty) in evicted {
                self.reporter.evict(victim_key, dirty, &stats);
            }
        }
        Ok(PinnedFrame::new(frame))
    }

    /// 追加一张零页（不变式 Ⅴ）。真正的"取页数 + 写零页"在 `DiskManager`
    /// 里由同一个文件锁保护，所以这里不需要额外同步。
    pub fn alloc_page(&self, file: FileId) -> Result<PageNo> {
        self.disk.alloc_page(file)
    }

    pub fn create_file(&self, path: &Path) -> Result<FileId> {
        self.disk.create_file(path)
    }

    pub fn open_file(&self, path: &Path) -> Result<FileId> {
        self.disk.open_file(path)
    }

    pub fn page_count(&self, file: FileId) -> Result<PageNo> {
        self.disk.page_count(file)
    }

    /// Empties a file in place. Cached frames of the file must be dropped
    /// first (see `discard_file`).
    pub fn truncate_file(&self, file: FileId) -> Result<()> {
        self.disk.truncate_file(file)
    }

    /// Drops all cached frames of a file without writing them back, closes
    /// its handle and returns the path for deletion.
    pub fn close_file(&self, file: FileId) -> Result<PathBuf> {
        self.discard_file(file);
        self.disk.close_file(file)
    }

    /// Drops all cached frames of a file without writing them back. A reader
    /// that already resolved a frame keeps its own `Arc`, so it is unaffected.
    ///
    /// 被 pin 的帧**不**被跳过：这是"这份数据已经不要了"的单写者契约，调用者
    /// （`rebuild_indexes` / `drop_table`）持库级写锁，因此正常情况下没有在飞的
    /// 闭包。真有的话，它的修改随最后一个 `Arc` 一起消失，这正是本方法要的语义。
    pub fn discard_file(&self, file: FileId) {
        let mut state = self.state.lock();
        let PoolState { frames, replacer } = &mut *state;
        let dropped: Vec<Key> = frames.keys().filter(|k| k.0 == file).copied().collect();
        frames.retain(|k, _| k.0 != file);
        // 逐键注销：策略只提供 `forget(&mut self, key)`，没有"按文件清空"，
        // 这样三种策略共用一条路径，也顺手维持了 `len()` 与页表同步。
        for key in dropped {
            replacer.forget(key);
        }
    }

    /// 只回写一个文件的脏帧，然后 `sync` 该文件。是 `flush_all` 的单文件版本，
    /// 目前**没有调用者**（checkpoint 走 `flush_all`），保留它是为了给按文件
    /// checkpoint 留出接缝 —— 也正因为有它，`dirty_frames` 才需要 `only` 参数。
    pub fn flush_file(&self, file: FileId) -> Result<()> {
        let _serial = self.flush_lock.lock();
        let frames = self.dirty_frames(Some(file));
        if frames.is_empty() {
            return Ok(());
        }
        for frame in frames {
            if frame.is_dirty() {
                // 锁序：页闩 → 文件锁，与淘汰路径同向（见 `frame_for` 的注释）。
                // 这里**不**持 `state`（`dirty_frames` 返回前已释放），比淘汰少一层。
                let data = frame.data();
                self.disk.write_page(frame.file(), frame.no(), &data)?;
                frame.clear_dirty();
            }
        }
        self.disk.sync_file(file)?;
        Ok(())
    }

    pub fn flush_all(&self) -> Result<()> {
        let _serial = self.flush_lock.lock();
        let frames = self.dirty_frames(None);
        if frames.is_empty() {
            return Ok(());
        }
        // double-write: stage every page and sync before touching the final
        // files, so a crash mid-write can be repaired on the next open
        // 锁序：页闩 → 文件锁，与 `frame_for` 的淘汰回写同向（那边外面还套着 `state`）。
        for frame in &frames {
            let data = frame.data();
            self.disk.stage_page(frame.file(), frame.no(), &data)?;
        }
        self.disk.sync_double_write()?;
        for frame in &frames {
            let data = frame.data();
            self.disk.write_page(frame.file(), frame.no(), &data)?;
            frame.clear_dirty();
        }
        // The final pages must be durable before the DWB can be discarded;
        // otherwise a crash after reset would lose them with no repair copy.
        let mut files: Vec<FileId> = frames.iter().map(|f| f.file()).collect();
        files.sort_unstable();
        files.dedup();
        for file in files {
            self.disk.sync_file(file)?;
        }
        self.disk.reset_double_write()?;
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

impl Drop for BufferPool {
    fn drop(&mut self) {
        let _ = self.flush_all();
        // final snapshot, so a short run that never checkpointed still reports
        self.report_stats();
    }
}
