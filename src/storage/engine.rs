use crate::storage::buffer::BufferPool;
use crate::storage::heap::{HeapFile, Rid};
use crate::storage::page::{FileId, PageNo};
use crate::storage::slotted::page_iter;
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
    buffer: std::vec::IntoIter<(Rid, Vec<u8>)>,
}

impl HeapScanner {
    fn new(bp: &BufferPool, file: FileId) -> Result<Self> {
        let pages = bp.page_count(file)?;
        Ok(Self { file, next_page: 1, last_page: pages, buffer: Vec::new().into_iter() })
    }
}

impl RowScanner for HeapScanner {
    fn next(&mut self, bp: &BufferPool) -> Result<Option<(Rid, Vec<u8>)>> {
        loop {
            if let Some(row) = self.buffer.next() {
                return Ok(Some(row));
            }
            if self.next_page >= self.last_page {
                return Ok(None);
            }
            let no = self.next_page;
            self.next_page += 1;
            let mut rows = Vec::new();
            bp.read_page(self.file, no, |page| {
                for (slot, rec) in page_iter(page) {
                    rows.push((Rid::new(no, slot), rec.to_vec()));
                }
                Ok(())
            })?;
            self.buffer = rows.into_iter();
        }
    }
}
