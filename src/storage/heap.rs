use crate::config::PageLayout;
use crate::storage::buffer::BufferPool;
use crate::storage::header::{self, FileKind};
use crate::storage::page::{FileId, PageNo};
use crate::storage::pax;
use crate::storage::slotted::{page_delete, page_get, page_insert, page_iter, page_write};
use crate::{Error, Result};

const MAGIC: [u8; 8] = *b"CHIDHEAP";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rid {
    pub page_no: PageNo,
    pub slot: u16,
}

impl Rid {
    pub fn new(page_no: PageNo, slot: u16) -> Self {
        Self { page_no, slot }
    }
}

pub struct HeapFile {
    file: FileId,
    layout: PageLayout,
}

fn layout_tag(layout: PageLayout) -> u8 {
    match layout {
        PageLayout::Row => 0,
        PageLayout::Pax => 1,
    }
}

fn page_layout(page: &[u8]) -> PageLayout {
    match page[header::HEADER_LEN] {
        1 => PageLayout::Pax,
        _ => PageLayout::Row,
    }
}

impl HeapFile {
    pub fn at(file: FileId, layout: PageLayout) -> Self {
        Self { file, layout }
    }

    /// Layout recorded in the file header (page 0).
    pub fn layout(&self) -> PageLayout {
        self.layout
    }

    pub fn init(bp: &BufferPool, file: FileId) -> Result<Self> {
        Self::init_with_layout(bp, file, PageLayout::Row)
    }

    pub fn init_with_layout(bp: &BufferPool, file: FileId, layout: PageLayout) -> Result<Self> {
        if bp.page_count(file)? != 0 {
            return Err(Error::Runtime("cannot init heap file: file not empty".into()));
        }
        let no = bp.alloc_page(file)?;
        bp.with_page(file, no, |page| {
            header::write_header(page, &MAGIC, FileKind::Heap);
            page[header::HEADER_LEN] = layout_tag(layout);
            Ok(())
        })?;
        Ok(Self { file, layout })
    }

    pub fn open(bp: &BufferPool, file: FileId) -> Result<Self> {
        if bp.page_count(file)? == 0 {
            return Err(Error::Runtime("cannot open heap file: file is empty".into()));
        }
        let layout = bp.read_page(file, 0, |page| {
            header::read_header(page, &MAGIC, FileKind::Heap)?;
            Ok(page_layout(page))
        })?;
        Ok(Self { file, layout })
    }

    /// Opens the file, re-initializing a header that a crash lost before it
    /// reached the disk. Returns true when the file was re-initialized.
    pub fn open_or_repair(bp: &BufferPool, file: FileId, expected: PageLayout) -> Result<bool> {
        if bp.page_count(file)? == 0 {
            Self::init_with_layout(bp, file, expected)?;
            return Ok(true);
        }
        let header_lost = bp.read_page(file, 0, |page| {
            Ok(page[0..8] != MAGIC && page.iter().all(|&b| b == 0))
        })?;
        if !header_lost {
            let layout = bp.read_page(file, 0, |page| {
                header::read_header(page, &MAGIC, FileKind::Heap)?;
                Ok(page_layout(page))
            })?;
            if layout != expected {
                return Err(Error::Runtime(format!(
                    "heap file layout {layout:?} does not match the catalog's {expected:?}"
                )));
            }
            return Ok(false);
        }
        bp.with_page(file, 0, |page| {
            header::write_header(page, &MAGIC, FileKind::Heap);
            page[header::HEADER_LEN] = layout_tag(expected);
            Ok(())
        })?;
        Ok(true)
    }

    pub fn file_id(&self) -> FileId {
        self.file
    }

    pub fn insert(&self, bp: &BufferPool, record: &[u8]) -> Result<Rid> {
        Ok(self.insert_with_hint(bp, 0, record)?.0)
    }

    /// Inserts `record`, trying `hint` (the page that accepted the previous
    /// record) first so appends do not rescan every earlier page. If the hinted
    /// page is full the remaining pages are still tried (wrapping around), so
    /// space freed by deletes is reused. Returns the rid and the page that
    /// accepted the record, for the caller to pass as the next hint.
    pub fn insert_with_hint(
        &self,
        bp: &BufferPool,
        hint: PageNo,
        record: &[u8],
    ) -> Result<(Rid, PageNo)> {
        let pages = bp.page_count(self.file)?;
        let start = if (1..pages).contains(&hint) { hint } else { 1 };
        for no in [start..pages, 1..start].into_iter().flatten() {
            if let Some(slot) = self.try_insert(bp, no, record)? {
                return Ok((Rid::new(no, slot), no));
            }
        }
        let no = bp.alloc_page(self.file)?;
        let slot = self.try_insert(bp, no, record)?.ok_or_else(|| {
            Error::Runtime(format!(
                "record too large ({record_len} bytes does not fit in a page)",
                record_len = record.len()
            ))
        })?;
        Ok((Rid::new(no, slot), no))
    }

    /// Attempts to place `record` on page `no`; `None` when the page is full.
    fn try_insert(&self, bp: &BufferPool, no: PageNo, record: &[u8]) -> Result<Option<u16>> {
        bp.with_page(self.file, no, |page| match self.layout {
            PageLayout::Row => match page_insert(page, record) {
                Ok(slot) => Ok(Some(slot)),
                Err(Error::PageFull) => Ok(None),
                Err(e) => Err(e),
            },
            PageLayout::Pax => match pax::insert(page, record) {
                Ok(slot) => Ok(Some(slot)),
                Err(Error::PageFull) => Ok(None),
                Err(e) => Err(e),
            },
        })
    }

    pub fn get(&self, bp: &BufferPool, rid: Rid) -> Result<Vec<u8>> {
        bp.read_page(self.file, rid.page_no, |page| match self.layout {
            PageLayout::Row => page_get(page, rid.slot)?
                .map(|r| r.to_vec())
                .ok_or_else(|| Error::Runtime(format!("no record at {rid:?}"))),
            PageLayout::Pax => {
                let mut out = Vec::new();
                if pax::read_record(page, rid.slot, None, &mut out) {
                    Ok(out)
                } else {
                    Err(Error::Runtime(format!("no record at {rid:?}")))
                }
            }
        })
    }

    pub fn delete(&self, bp: &BufferPool, rid: Rid) -> Result<()> {
        bp.with_page(self.file, rid.page_no, |page| match self.layout {
            PageLayout::Row => page_delete(page, rid.slot),
            PageLayout::Pax => pax::delete(page, rid.slot),
        })
    }

    /// MVCC delete-mark: rewrites the record in place, setting its deleter id
    /// and forward pointer. Returns `(previous deleter, previous next_rid)`,
    /// which rollback and the conflict check need.
    pub fn delete_mark(
        &self,
        bp: &BufferPool,
        rid: Rid,
        deleter: u64,
        next_rid: u64,
    ) -> Result<(u64, u64)> {
        bp.with_page(self.file, rid.page_no, |page| match self.layout {
            PageLayout::Row => {
                let rec = page_get(page, rid.slot)?
                    .ok_or_else(|| Error::Runtime(format!("no record at {rid:?}")))?;
                let mut updated = rec.to_vec();
                if updated.len() < crate::storage::codec::RECORD_HEADER {
                    return Err(Error::Runtime("record lacks mvcc fields".into()));
                }
                let prev = u64::from_le_bytes(updated[8..16].try_into().unwrap());
                let prev_next = u64::from_le_bytes(updated[16..24].try_into().unwrap());
                updated[8..16].copy_from_slice(&deleter.to_le_bytes());
                updated[16..24].copy_from_slice(&next_rid.to_le_bytes());
                page_write(page, rid.slot, &updated)?;
                Ok((prev, prev_next))
            }
            PageLayout::Pax => pax::delete_mark(page, rid.slot, deleter, next_rid),
        })
    }

    pub fn for_each(
        &self,
        bp: &BufferPool,
        mut f: impl FnMut(Rid, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let pages = bp.page_count(self.file)?;
        for no in 1..pages {
            bp.read_page(self.file, no, |page| match self.layout {
                PageLayout::Row => {
                    for (slot, rec) in page_iter(page) {
                        f(Rid::new(no, slot), rec)?;
                    }
                    Ok(())
                }
                PageLayout::Pax => {
                    let mut buf = Vec::new();
                    for slot in pax::alive_slots(page) {
                        pax::read_record(page, slot, None, &mut buf);
                        f(Rid::new(no, slot), &buf)?;
                    }
                    Ok(())
                }
            })?;
        }
        Ok(())
    }
}
