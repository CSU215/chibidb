use crate::config::PageLayout;
use crate::storage::buffer::BufferPool;
use crate::storage::codec::{decode_row_with_want, decode_tagged_value, LobResolver};
use crate::storage::heap::{HeapFile, Rid};
use crate::storage::page::{FileId, PageNo, PAGE_SIZE};
use crate::storage::slotted::{page_get, page_iter, page_put_at, page_slots};
use crate::value::Value;
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

    /// Like [`TableEngine::scan`], but the caller promises it only reads the
    /// columns where `keep` is `true`; other columns may come back as `NULL`.
    /// A column-major engine uses this to skip unread columns entirely; the
    /// default ignores the hint and scans everything.
    fn scan_projected(&self, bp: &BufferPool, _keep: &[bool]) -> Result<Box<dyn RowScanner>> {
        self.scan(bp)
    }

    /// Pushes each row's requested base columns as `(creator, deleter,
    /// values)`, where `values[i]` is column `cols[i]`. Reading happens in
    /// place (no page image is copied) and unrequested columns are never
    /// built. An empty `cols` streams versions only (for `count(*)`). Returns
    /// `Ok(false)` when the engine has no columnar path; the caller applies
    /// MVCC visibility.
    fn for_each_projected(
        &self,
        _bp: &BufferPool,
        _cols: &[usize],
        _lobs: &dyn LobResolver,
        _sink: &mut dyn RowSink,
    ) -> Result<bool> {
        Ok(false)
    }

    /// Point fetch of one encoded record by row id.
    fn get(&self, bp: &BufferPool, rid: Rid) -> Result<Vec<u8>>;
}

/// Sink for [`TableEngine::for_each_projected`]. A trait rather than an
/// `FnMut` so the borrowed `values` slice does not trip higher-ranked-lifetime
/// inference when a caller wraps another closure.
pub trait RowSink {
    fn row(&mut self, creator: u32, deleter: u32, values: &[Value]) -> Result<()>;
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
    layout: PageLayout,
}

impl HeapEngine {
    pub fn new(file: FileId) -> Self {
        Self { file, layout: PageLayout::Row }
    }

    pub fn with_layout(file: FileId, layout: PageLayout) -> Self {
        Self { file, layout }
    }
}

impl TableEngine for HeapEngine {
    fn scan(&self, bp: &BufferPool) -> Result<Box<dyn RowScanner>> {
        Ok(Box::new(HeapScanner::new(bp, self.file, self.layout, None)?))
    }

    fn scan_projected(&self, bp: &BufferPool, keep: &[bool]) -> Result<Box<dyn RowScanner>> {
        match self.layout {
            // Row pages store whole records, so there is nothing to skip.
            PageLayout::Row => self.scan(bp),
            PageLayout::Pax => {
                Ok(Box::new(HeapScanner::new(bp, self.file, self.layout, Some(keep.to_vec()))?))
            }
        }
    }

    fn for_each_projected(
        &self,
        bp: &BufferPool,
        cols: &[usize],
        lobs: &dyn LobResolver,
        sink: &mut dyn RowSink,
    ) -> Result<bool> {
        // Column -> output slot, built once; `usize::MAX` marks a skipped one.
        let max = cols.iter().copied().max().map_or(0, |m| m + 1);
        let mut want = vec![usize::MAX; max];
        for (slot, &c) in cols.iter().enumerate() {
            if c < max {
                want[c] = slot;
            }
        }
        let mut out: Vec<Value> = vec![Value::Null; cols.len()];
        let pages = bp.page_count(self.file)?;
        for no in 1..pages {
            // The page latch is held only for this page's rows; nothing is
            // copied out of the pool.
            bp.read_page(self.file, no, |page| {
                match self.layout {
                    PageLayout::Row => {
                        for (_, rec) in page_iter(page) {
                            if rec.len() < 8 {
                                return Err(Error::Runtime("truncated versioned record".into()));
                            }
                            let creator = u32::from_le_bytes(rec[0..4].try_into().unwrap());
                            let deleter = u32::from_le_bytes(rec[4..8].try_into().unwrap());
                            if !cols.is_empty() {
                                out.iter_mut().for_each(|v| *v = Value::Null);
                                decode_row_with_want(&rec[8..], &want, lobs, &mut out)?;
                            }
                            sink.row(creator, deleter, &out)?;
                        }
                    }
                    PageLayout::Pax => {
                        for slot in crate::storage::pax::alive_slots(page) {
                            let (creator, deleter) =
                                crate::storage::pax::version_at(page, slot);
                            for (pos, &c) in cols.iter().enumerate() {
                                out[pos] = decode_tagged_value(
                                    crate::storage::pax::column_bytes(page, c, slot),
                                    lobs,
                                )?;
                            }
                            sink.row(creator, deleter, &out)?;
                        }
                    }
                }
                Ok(())
            })?;
        }
        Ok(true)
    }

    fn get(&self, bp: &BufferPool, rid: Rid) -> Result<Vec<u8>> {
        HeapFile::at(self.file, self.layout).get(bp, rid)
    }
}

impl TableStorage for HeapEngine {
    fn insert(&self, bp: &BufferPool, record: &[u8]) -> Result<Rid> {
        HeapFile::at(self.file, self.layout).insert(bp, record)
    }

    fn delete(&self, bp: &BufferPool, rid: Rid) -> Result<()> {
        HeapFile::at(self.file, self.layout).delete(bp, rid)
    }

    fn delete_mark(&self, bp: &BufferPool, rid: Rid, deleter: u32) -> Result<u32> {
        HeapFile::at(self.file, self.layout).delete_mark(bp, rid, deleter)
    }

    fn file_id(&self) -> FileId {
        self.file
    }

    /// Replays a WAL insert at its original rid. Heap pages are page-addressed,
    /// so the page is allocated and the record placed, unless it is already
    /// present (replay is idempotent).
    fn insert_at(&self, bp: &BufferPool, rid: Rid, record: &[u8]) -> Result<()> {
        while bp.page_count(self.file)? <= rid.page_no {
            bp.alloc_page(self.file)?;
        }
        let occupied = bp.read_page(self.file, rid.page_no, |page| match self.layout {
            PageLayout::Row => Ok(page_get(page, rid.slot)?.is_some()),
            PageLayout::Pax => Ok(!crate::storage::pax::is_empty(page, rid.slot)),
        })?;
        if occupied {
            return Ok(());
        }
        bp.with_page(self.file, rid.page_no, |page| match self.layout {
            PageLayout::Row => page_put_at(page, rid.slot, record),
            PageLayout::Pax => crate::storage::pax::put_at(page, rid.slot, record),
        })
    }
}

struct HeapScanner {
    file: FileId,
    layout: PageLayout,
    /// Columns the caller reads; `None` reads every column. Only PAX pages
    /// use this, to avoid touching unread column segments.
    keep: Option<Vec<bool>>,
    next_page: PageNo,
    last_page: PageNo,
    /// Image of the page currently being drained; reused across pages.
    page: Vec<u8>,
    page_no: PageNo,
    /// `(slot, offset, length)` of each live record in `page`; PAX records
    /// reconstruct from columns, so their offset/length stay zero.
    slots: Vec<(u16, usize, usize)>,
    slot_pos: usize,
}

impl HeapScanner {
    fn new(
        bp: &BufferPool,
        file: FileId,
        layout: PageLayout,
        keep: Option<Vec<bool>>,
    ) -> Result<Self> {
        let pages = bp.page_count(file)?;
        Ok(Self {
            file,
            layout,
            keep,
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
            match self.layout {
                PageLayout::Row => self.slots.extend(page_slots(&self.page)),
                PageLayout::Pax => self
                    .slots
                    .extend(crate::storage::pax::alive_slots(&self.page).map(|s| (s, 0, 0))),
            }
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

    fn record_into(&self, rid: Rid, off: usize, len: usize, out: &mut Vec<u8>) {
        out.clear();
        match self.layout {
            PageLayout::Row => out.extend_from_slice(&self.page[off..off + len]),
            PageLayout::Pax => {
                crate::storage::pax::read_record(&self.page, rid.slot, self.keep.as_deref(), out);
            }
        }
    }
}

impl RowScanner for HeapScanner {
    fn next(&mut self, bp: &BufferPool) -> Result<Option<(Rid, Vec<u8>)>> {
        match self.take(bp)? {
            Some((rid, off, len)) => {
                let mut out = Vec::new();
                self.record_into(rid, off, len, &mut out);
                Ok(Some((rid, out)))
            }
            None => Ok(None),
        }
    }

    fn next_into(&mut self, bp: &BufferPool, out: &mut Vec<u8>) -> Result<Option<Rid>> {
        match self.take(bp)? {
            Some((rid, off, len)) => {
                self.record_into(rid, off, len, out);
                Ok(Some(rid))
            }
            None => Ok(None),
        }
    }
}
