use crate::storage::buffer::BufferPool;
use crate::storage::header::{self, FileKind};
use crate::storage::page::{FileId, PageNo};
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
}

impl HeapFile {
    pub fn at(file: FileId) -> Self {
        Self { file }
    }

    pub fn init(bp: &BufferPool, file: FileId) -> Result<Self> {
        if bp.page_count(file)? != 0 {
            return Err(Error::Runtime("cannot init heap file: file not empty".into()));
        }
        let no = bp.alloc_page(file)?;
        bp.with_page(file, no, |page| {
            header::write_header(page, &MAGIC, FileKind::Heap);
            Ok(())
        })?;
        Ok(Self { file })
    }

    pub fn open(bp: &BufferPool, file: FileId) -> Result<Self> {
        if bp.page_count(file)? == 0 {
            return Err(Error::Runtime("cannot open heap file: file is empty".into()));
        }
        bp.read_page(file, 0, |page| header::read_header(page, &MAGIC, FileKind::Heap))?;
        Ok(Self { file })
    }

    /// Opens the file, re-initializing a header that a crash lost before it
    /// reached the disk. Returns true when the file was re-initialized.
    pub fn open_or_repair(bp: &BufferPool, file: FileId) -> Result<bool> {
        if bp.page_count(file)? == 0 {
            Self::init(bp, file)?;
            return Ok(true);
        }
        let header_lost = bp.read_page(file, 0, |page| {
            Ok(page[0..8] != MAGIC && page.iter().all(|&b| b == 0))
        })?;
        if !header_lost {
            Self::open(bp, file)?;
            return Ok(false);
        }
        bp.with_page(file, 0, |page| {
            header::write_header(page, &MAGIC, FileKind::Heap);
            Ok(())
        })?;
        Ok(true)
    }

    pub fn file_id(&self) -> FileId {
        self.file
    }

    pub fn insert(&self, bp: &BufferPool, record: &[u8]) -> Result<Rid> {
        let pages = bp.page_count(self.file)?;
        for no in 1..pages {
            match bp.with_page(self.file, no, |page| page_insert(page, record)) {
                Ok(slot) => return Ok(Rid::new(no, slot)),
                Err(Error::PageFull) => continue,
                Err(e) => return Err(e),
            }
        }
        let no = bp.alloc_page(self.file)?;
        let slot = bp.with_page(self.file, no, |page| page_insert(page, record))
            .map_err(|e| {
                if matches!(e, Error::PageFull) {
                    Error::Runtime(format!(
                        "record too large ({record_len} bytes does not fit in a page)",
                        record_len = record.len()
                    ))
                } else {
                    e
                }
            })?;
        Ok(Rid::new(no, slot))
    }

    pub fn get(&self, bp: &BufferPool, rid: Rid) -> Result<Vec<u8>> {
        bp.read_page(self.file, rid.page_no, |page| {
            page_get(page, rid.slot)?
                .map(|r| r.to_vec())
                .ok_or_else(|| Error::Runtime(format!("no record at {rid:?}")))
        })
    }

    pub fn delete(&self, bp: &BufferPool, rid: Rid) -> Result<()> {
        bp.with_page(self.file, rid.page_no, |page| page_delete(page, rid.slot))
    }

    /// MVCC delete-mark: rewrites the record in place, setting its deleter id.
    /// Returns the previous deleter (0 when the version was live), which the
    /// first-committer-wins check needs.
    pub fn delete_mark(&self, bp: &BufferPool, rid: Rid, deleter: u32) -> Result<u32> {
        bp.with_page(self.file, rid.page_no, |page| {
            let rec = page_get(page, rid.slot)?
                .ok_or_else(|| Error::Runtime(format!("no record at {rid:?}")))?;
            let mut updated = rec.to_vec();
            if updated.len() < 8 {
                return Err(Error::Runtime("record lacks mvcc fields".into()));
            }
            let prev = u32::from_le_bytes(updated[4..8].try_into().unwrap());
            updated[4..8].copy_from_slice(&deleter.to_le_bytes());
            page_write(page, rid.slot, &updated)?;
            Ok(prev)
        })
    }

    pub fn for_each(
        &self,
        bp: &BufferPool,
        mut f: impl FnMut(Rid, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let pages = bp.page_count(self.file)?;
        for no in 1..pages {
            bp.read_page(self.file, no, |page| {
                for (slot, rec) in page_iter(page) {
                    f(Rid::new(no, slot), rec)?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }
}
