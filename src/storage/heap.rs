use crate::storage::buffer::BufferPool;
use crate::storage::page::{FileId, PageNo};
use crate::storage::slotted::{page_delete, page_get, page_insert, page_iter};
use crate::{Error, Result};

const MAGIC: [u8; 4] = *b"CHID";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    pub fn init(bp: &mut BufferPool, file: FileId) -> Result<Self> {
        if bp.page_count(file)? != 0 {
            return Err(Error::Runtime("cannot init heap file: file not empty".into()));
        }
        let no = bp.alloc_page(file)?;
        bp.with_page(file, no, |page| {
            page[0..4].copy_from_slice(&MAGIC);
            Ok(())
        })?;
        Ok(Self { file })
    }

    pub fn open(bp: &mut BufferPool, file: FileId) -> Result<Self> {
        if bp.page_count(file)? == 0 {
            return Err(Error::Runtime("cannot open heap file: file is empty".into()));
        }
        bp.read_page(file, 0, |page| {
            if page[0..4] == MAGIC {
                Ok(())
            } else {
                Err(Error::Runtime("not a chibidb data file".into()))
            }
        })?;
        Ok(Self { file })
    }

    pub fn file_id(&self) -> FileId {
        self.file
    }

    pub fn insert(&self, bp: &mut BufferPool, record: &[u8]) -> Result<Rid> {
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

    pub fn get(&self, bp: &mut BufferPool, rid: Rid) -> Result<Vec<u8>> {
        bp.read_page(self.file, rid.page_no, |page| {
            page_get(page, rid.slot)?
                .map(|r| r.to_vec())
                .ok_or_else(|| Error::Runtime(format!("no record at {rid:?}")))
        })
    }

    pub fn delete(&self, bp: &mut BufferPool, rid: Rid) -> Result<()> {
        bp.with_page(self.file, rid.page_no, |page| page_delete(page, rid.slot))
    }

    pub fn for_each(
        &self,
        bp: &mut BufferPool,
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
