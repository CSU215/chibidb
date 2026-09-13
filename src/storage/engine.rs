use crate::storage::buffer::BufferPool;
use crate::storage::heap::{HeapFile, Rid};
use crate::storage::page::{FileId, PageNo, PAGE_SIZE};
use crate::storage::slotted::page_slots;
use crate::{Error, Result};
/// A forward-only cursor over a table's rows, decoupled from the concrete
/// storage engine.
///
/// The buffer pool is passed on each call instead of being borrowed for the
/// scanner's lifetime, so several scanners can be interleaved (e.g. a
/// nested-loop join) without aliasing the pool.
pub trait RowScanner: Send {
    /// Returns the next `(Rid, encoded record)` pair, or `None` at EOF.
    fn next(&mut self, bp: &BufferPool) -> Result<Option<(Rid, Vec<u8>)>>;

    /// Returns the next record's row id, writing its encoded bytes into `out`
    /// (cleared first). Lets a scan decode from one reused buffer instead of
    /// allocating per row; the default copies from [`RowScanner::next`].
    fn next_into(&mut self, bp: &BufferPool, out: &mut Vec<u8>) -> Result<Option<Rid>> {
        match self.next(bp)? {
            Some((rid, record)) => {
                out.clear();
                out.extend_from_slice(&record);
                Ok(Some(rid))
            }
            None => Ok(None),
        }
    }

    /// Fills a batch of up to `max` records (fewer only at EOF).
    ///
    /// The default drains [`RowScanner::next`]; the heap already reads whole
    /// pages into an internal buffer, so this still batches page I/O.
    fn next_batch(&mut self, bp: &BufferPool, max: usize) -> Result<Vec<(Rid, Vec<u8>)>> {
        let mut batch = Vec::new();
        while batch.len() < max {
            match self.next(bp)? {
                Some(entry) => batch.push(entry),
                None => break,
            }
        }
        Ok(batch)
    }
}

/// Read seam over a table's storage. The executor depends on this, not on the
/// concrete engine (heap today, LSM later).
pub trait TableEngine: Send + Sync {
    fn scan(&self, bp: &BufferPool) -> Result<Box<dyn RowScanner>>;

    /// Point fetch of one encoded record by row id.
    fn get(&self, bp: &BufferPool, rid: Rid) -> Result<Vec<u8>>;
}

/// Full table-storage seam: MVCC version writes plus the read cursor. The
/// heap is the only implementation today; an LSM engine will implement the
/// same surface with its own versioning.
pub trait TableStorage: TableEngine + std::fmt::Debug {
    /// Appends a versioned record and returns its row id.
    fn insert(&self, bp: &BufferPool, record: &[u8]) -> Result<Rid>;

    /// Physically removes a record (rollback and vacuum).
    fn delete(&self, bp: &BufferPool, rid: Rid) -> Result<()>;

    /// Rewrites the record's deleter field; returns the previous deleter,
    /// which first-committer-wins needs.
    fn delete_mark(&self, bp: &BufferPool, rid: Rid, deleter: u32) -> Result<u32>;

    /// The file backing this table, for WAL replay and index mapping.
    fn file_id(&self) -> FileId;

    /// Places an already-encoded version at an exact row id during WAL replay.
    /// Engines without page-addressed storage (LSM) override this; the heap
    /// replays by writing pages directly.
    fn insert_at(&self, bp: &BufferPool, rid: Rid, record: &[u8]) -> Result<()> {
        let _ = (bp, rid, record);
        Err(Error::Runtime("insert_at is not supported by this engine".into()))
    }

    /// Makes pending writes durable. The heap's pages reach disk through the
    /// buffer pool, so its default is a no-op; the LSM engine flushes its
    /// memtable to an SSTable.
    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// Engine backed by the current on-disk heap layout. `new` is an unvalidated
/// handle; callers rely on the catalog having opened/validated the file.
#[derive(Debug)]
pub struct HeapEngine {
    file: FileId,
}

impl HeapEngine {
    pub fn new(file: FileId) -> Self {
        Self { file }
    }
}

impl TableEngine for HeapEngine {
    fn scan(&self, bp: &BufferPool) -> Result<Box<dyn RowScanner>> {
        Ok(Box::new(HeapScanner::new(bp, self.file)?))
    }

    fn get(&self, bp: &BufferPool, rid: Rid) -> Result<Vec<u8>> {
        HeapFile::at(self.file).get(bp, rid)
    }
}

impl TableStorage for HeapEngine {
    fn insert(&self, bp: &BufferPool, record: &[u8]) -> Result<Rid> {
        HeapFile::at(self.file).insert(bp, record)
    }

    fn delete(&self, bp: &BufferPool, rid: Rid) -> Result<()> {
        HeapFile::at(self.file).delete(bp, rid)
    }

    fn delete_mark(&self, bp: &BufferPool, rid: Rid, deleter: u32) -> Result<u32> {
        HeapFile::at(self.file).delete_mark(bp, rid, deleter)
    }

    fn file_id(&self) -> FileId {
        self.file
    }
}

struct HeapScanner {
    file: FileId,
    next_page: PageNo,
    last_page: PageNo,
    /// Image of the page currently being drained; reused across pages.
    page: Vec<u8>,
    page_no: PageNo,
    /// `(slot, offset, length)` of each live record in `page`.
    slots: Vec<(u16, usize, usize)>,
    slot_pos: usize,
}

impl HeapScanner {
    fn new(bp: &BufferPool, file: FileId) -> Result<Self> {
        let pages = bp.page_count(file)?;
        Ok(Self {
            file,
            next_page: 1,
            last_page: pages,
            page: Vec::new(),
            page_no: 0,
            slots: Vec::new(),
            slot_pos: 0,
        })
    }

    /// Advances to the next page holding a live record; `false` at EOF.
    fn advance(&mut self, bp: &BufferPool) -> Result<bool> {
        while self.slot_pos >= self.slots.len() {
            if self.next_page >= self.last_page {
                return Ok(false);
            }
            let no = self.next_page;
            self.next_page += 1;
            if self.page.len() != PAGE_SIZE {
                self.page.resize(PAGE_SIZE, 0);
            }
            bp.read_page(self.file, no, |data| {
                self.page.copy_from_slice(data);
                Ok(())
            })?;
            self.page_no = no;
            self.slots.clear();
            self.slots.extend(page_slots(&self.page));
            self.slot_pos = 0;
        }
        Ok(true)
    }

    /// Pops `(rid, offset, length)` of the next record.
    fn take(&mut self, bp: &BufferPool) -> Result<Option<(Rid, usize, usize)>> {
        if !self.advance(bp)? {
            return Ok(None);
        }
        let (slot, off, len) = self.slots[self.slot_pos];
        self.slot_pos += 1;
        Ok(Some((Rid::new(self.page_no, slot), off, len)))
    }
}

impl RowScanner for HeapScanner {
    fn next(&mut self, bp: &BufferPool) -> Result<Option<(Rid, Vec<u8>)>> {
        match self.take(bp)? {
            Some((rid, off, len)) => Ok(Some((rid, self.page[off..off + len].to_vec()))),
            None => Ok(None),
        }
    }

    fn next_into(&mut self, bp: &BufferPool, out: &mut Vec<u8>) -> Result<Option<Rid>> {
        match self.take(bp)? {
            Some((rid, off, len)) => {
                out.clear();
                out.extend_from_slice(&self.page[off..off + len]);
                Ok(Some(rid))
            }
            None => Ok(None),
        }
    }
}
