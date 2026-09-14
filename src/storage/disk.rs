use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use parking_lot::{Mutex, RwLock};

use crate::storage::dwb::DoubleWrite;
use crate::storage::page::{FileId, PageNo, PAGE_SIZE};
use crate::{Error, Result};

/// 一个已打开文件的槽位。
///
/// `Arc` 是这里的重点：调用方**短持** `files` 锁，取出 `Arc` 后立刻释放，
/// 再去锁这个文件自己的 `Mutex<Option<File>>`。于是"map 锁"与"文件锁"之间
/// 没有嵌套，锁序问题从根上消失，而且持文件锁做 I/O 的线程不会挡住
/// `open_file` / `close_file`（后者要写锁）。
struct FileSlot {
    path: PathBuf,
    /// `None` 表示句柄已关闭（见 `close_file`）。用 `Option` 而不是裸 `File`，
    /// 是为了让"关闭"这个动作与 `Arc` 的存活解耦。
    file: Mutex<Option<File>>,
}

/// 分页文件的物理 I/O 层。所有方法取 `&self`：内部锁已经做到按文件划分，
/// 不同文件的读写完全并行，同一文件由它自己的 `Mutex<File>` 串行。
///
/// 锁序（本文件内唯一的嵌套是 `files` → `文件锁`，且只在 `with_file` 的一瞬间）：
/// `FileSlot` 取出后 `files` 的读锁即刻释放，所以这里**不存在**两把锁同时持有的路径。
/// 这一层再往外看，调用方（`BufferPool`）的顺序是
/// `state → 页闩 → 文件锁`（淘汰回写）与 `页闩 → 文件锁`（flush），
/// 两条路径在"页闩 → 文件锁"这一段同向，因此不会成环。
pub struct DiskManager {
    files: RwLock<BTreeMap<FileId, Arc<FileSlot>>>,
    next_file_id: AtomicU32,
    /// Double-Write Buffer 是一个 db 目录一个的全局单例，单独一把锁，
    /// 与 `files` 不嵌套（`stage_page` 先取出 `Arc<FileSlot>` 再锁 dwb）。
    dwb: Mutex<Option<DoubleWrite>>,
}

impl Default for DiskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DiskManager {
    pub fn new() -> Self {
        Self {
            files: RwLock::new(BTreeMap::new()),
            next_file_id: AtomicU32::new(0),
            dwb: Mutex::new(None),
        }
    }

    /// Enables a double-write buffer: dirty pages are staged there and synced
    /// before reaching their final location, so a torn final write can be
    /// repaired on the next open.
    pub fn enable_double_write(&self, path: &Path) -> Result<()> {
        *self.dwb.lock() = Some(DoubleWrite::open(path)?);
        Ok(())
    }

    /// Stages a page into the double-write buffer (no-op when disabled).
    pub fn stage_page(&self, file: FileId, no: PageNo, buf: &[u8; PAGE_SIZE]) -> Result<()> {
        let slot = self.slot(file)?;
        let mut dwb = self.dwb.lock();
        if let Some(dwb) = dwb.as_mut() {
            dwb.stage(&slot.path, no, buf)?;
        }
        Ok(())
    }

    pub fn sync_double_write(&self) -> Result<()> {
        if let Some(dwb) = self.dwb.lock().as_mut() {
            dwb.sync()?;
        }
        Ok(())
    }

    pub fn reset_double_write(&self) -> Result<()> {
        if let Some(dwb) = self.dwb.lock().as_mut() {
            dwb.reset()?;
        }
        Ok(())
    }

    pub fn create_file(&self, path: &Path) -> Result<FileId> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| Error::Runtime(format!("cannot create file {}: {e}", path.display())))?;
        Ok(self.register(path, file))
    }

    pub fn open_file(&self, path: &Path) -> Result<FileId> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| Error::Runtime(format!("cannot open file {}: {e}", path.display())))?;
        Ok(self.register(path, file))
    }

    fn register(&self, path: &Path, file: File) -> FileId {
        let id = self.next_file_id.fetch_add(1, Ordering::Relaxed);
        let slot = Arc::new(FileSlot { path: path.to_path_buf(), file: Mutex::new(Some(file)) });
        self.files.write().insert(id, slot);
        id
    }

    /// Closes the handle of a file that is about to be deleted and returns
    /// its path (Windows cannot delete a file while a handle is open).
    ///
    /// 句柄是**在这里**被析构的，不依赖 `Arc` 的最后一个引用何时消失——
    /// 否则一个正在飞的 `with_file` 会让调用方随后删文件失败。
    /// 那个在飞的调用会读到 `None` 并报错，这是"文件已关闭"的正确语义。
    pub fn close_file(&self, file: FileId) -> Result<PathBuf> {
        let slot = self
            .files
            .write()
            .remove(&file)
            .ok_or_else(|| Error::Runtime(format!("unknown file id {file}")))?;
        let _ = slot.file.lock().take();
        Ok(slot.path.clone())
    }

    /// 在指定文件上跑一段 I/O：闭包拿到句柄与路径，期间该文件的锁被持有。
    ///
    /// 这是本层唯一对外暴露"持锁做任意 I/O"的入口，`BufferPool` 的按文件
    /// checkpoint 之类需要跨多个 `read_page`/`write_page` 保持原子的场景可以用它。
    /// 闭包里**不要**再调本结构的其它方法（那会重入同一把文件锁）。
    pub fn with_file<T>(
        &self,
        file: FileId,
        f: impl FnOnce(&mut File, &Path) -> Result<T>,
    ) -> Result<T> {
        let slot = self.slot(file)?;
        let mut guard = slot.file.lock();
        let handle = guard
            .as_mut()
            .ok_or_else(|| Error::Runtime(format!("file id {file} is closed")))?;
        f(handle, &slot.path)
    }

    pub fn read_page(&self, file: FileId, no: PageNo, buf: &mut [u8; PAGE_SIZE]) -> Result<()> {
        self.with_file(file, |f, _| {
            let offset = no as u64 * PAGE_SIZE as u64;
            let len = f.metadata().map_err(io_err)?.len();
            buf.fill(0);
            if offset >= len {
                return Ok(());
            }
            f.seek(SeekFrom::Start(offset)).map_err(io_err)?;
            f.read_exact(buf).map_err(io_err)?;
            Ok(())
        })
    }

    pub fn write_page(&self, file: FileId, no: PageNo, buf: &[u8; PAGE_SIZE]) -> Result<()> {
        self.with_file(file, |f, _| {
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
            f.write_all(buf).map_err(io_err)
            // 这里**不**调用 `File::flush`：标准库文档明确它是 no-op，
            // 只会让读者以为这一页已经落盘。耐久性由 `sync_file`、WAL 与
            // Double-Write Buffer 负责（不变式 Ⅺ）。
        })
    }

    /// 追加一张零页，返回它的页号（不变式 Ⅴ）。
    ///
    /// "取页数 → 写零页"两步在**同一个文件锁**内完成，所以并发追加同一文件
    /// 也不会拿到重复页号。`BufferPool::alloc_page` 直接转发到这里。
    pub fn alloc_page(&self, file: FileId) -> Result<PageNo> {
        self.with_file(file, |f, _| {
            let len = f.metadata().map_err(io_err)?.len();
            let no = (len / PAGE_SIZE as u64) as PageNo;
            f.seek(SeekFrom::Start(len)).map_err(io_err)?;
            f.write_all(&[0u8; PAGE_SIZE]).map_err(io_err)?;
            Ok(no)
        })
    }

    pub fn page_count(&self, file: FileId) -> Result<PageNo> {
        self.with_file(file, |f, _| {
            let len = f.metadata().map_err(io_err)?.len();
            Ok((len / PAGE_SIZE as u64) as PageNo)
        })
    }

    /// Flushes a file's data and metadata to stable storage. Used by the
    /// buffer pool after writing final pages and before the WAL/double-write
    /// buffer may be discarded.
    pub fn sync_file(&self, file: FileId) -> Result<()> {
        self.with_file(file, |f, _| f.sync_all().map_err(io_err))
    }

    /// Empties a file in place (used when rebuilding derived structures).
    pub fn truncate_file(&self, file: FileId) -> Result<()> {
        self.with_file(file, |f, _| f.set_len(0).map_err(io_err))
    }

    /// 取出槽位的 `Arc`。`files` 的读锁在返回前就已释放。
    fn slot(&self, file: FileId) -> Result<Arc<FileSlot>> {
        self.files
            .read()
            .get(&file)
            .cloned()
            .ok_or_else(|| Error::Runtime(format!("unknown file id {file}")))
    }
}

fn io_err(e: std::io::Error) -> Error {
    Error::Runtime(format!("io error: {e}"))
}
