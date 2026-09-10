use crate::storage::buffer::BufferPool;
use crate::storage::heap::Rid;
use crate::storage::page::{FileId, PageNo};
use crate::storage::slotted::page_iter;
use crate::Result;
/// A forward-only cursor over a table's rows, decoupled from the concrete
/// storage engine.
///
/// The buffer pool is passed on each call instead of being borrowed for the
/// scanner's lifetime, so several scanners can be interleaved (e.g. a
/// nested-loop join) without aliasing the pool.
pub trait RowScanner: Send {
    /// Returns the next `(Rid, encoded record)` pair, or `None` at EOF.
    fn next(&mut self, bp: &mut BufferPool) -> Result<Option<(Rid, Vec<u8>)>>;
}

/// Read seam over a table's storage. The executor depends on this, not on the
/// concrete engine (heap today, LSM later).
pub trait TableEngine: Send + Sync {
    fn scan(&self, bp: &mut BufferPool) -> Result<Box<dyn RowScanner>>;
}

/// Engine backed by the current on-disk heap layout. `new` is an unvalidated
/// handle; callers rely on the catalog having opened/validated the file.
pub struct HeapEngine {
    file: FileId,
}

impl HeapEngine {
    pub fn new(file: FileId) -> Self {
        Self { file }
    }
}

impl TableEngine for HeapEngine {
    fn scan(&self, bp: &mut BufferPool) -> Result<Box<dyn RowScanner>> {
        Ok(Box::new(HeapScanner::new(bp, self.file)?))
    }
}

struct HeapScanner {
    file: FileId,
    next_page: PageNo,
    last_page: PageNo,
    buffer: std::vec::IntoIter<(Rid, Vec<u8>)>,
}

impl HeapScanner {
    fn new(bp: &mut BufferPool, file: FileId) -> Result<Self> {
        let pages = bp.page_count(file)?;
        Ok(Self { file, next_page: 1, last_page: pages, buffer: Vec::new().into_iter() })
    }
}

impl RowScanner for HeapScanner {
    fn next(&mut self, bp: &mut BufferPool) -> Result<Option<(Rid, Vec<u8>)>> {
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
