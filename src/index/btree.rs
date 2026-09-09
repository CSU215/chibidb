use crate::index::node::{internal_child_for, leaf_init, leaf_insert_at, leaf_lower_bound, node_type, INTERNAL, LEAF};
use crate::storage::buffer::BufferPool;
use crate::storage::page::{FileId, PageNo};
use crate::storage::Rid;
use crate::{Error, Result};

const MAGIC: [u8; 8] = *b"CHIDBTX1";

pub struct BTree {
    file: FileId,
}

impl BTree {
    pub fn init(bp: &mut BufferPool, file: FileId) -> Result<Self> {
        if bp.page_count(file)? != 0 {
            return Err(Error::Runtime("cannot init index file: file not empty".into()));
        }
        let no = bp.alloc_page(file)?;
        bp.with_page(file, no, |page| {
            page[0..8].copy_from_slice(&MAGIC);
            u32_put(page, 8, 0); // root: empty tree
            u32_put(page, 12, 0); // first leaf
            Ok(())
        })?;
        Ok(Self { file })
    }

    pub fn open(bp: &mut BufferPool, file: FileId) -> Result<Self> {
        if bp.page_count(file)? == 0 {
            return Err(Error::Runtime("cannot open index file: file is empty".into()));
        }
        bp.read_page(file, 0, |page| {
            if page[0..8] == MAGIC {
                Ok(())
            } else {
                Err(Error::Runtime("not a chibidb index file".into()))
            }
        })?;
        Ok(Self { file })
    }

    pub fn file_id(&self) -> FileId {
        self.file
    }

    fn header_u32(&self, bp: &mut BufferPool, off: usize) -> Result<PageNo> {
        bp.read_page(self.file, 0, |page| Ok(u32_get(page, off)))
    }

    fn root(&self, bp: &mut BufferPool) -> Result<PageNo> {
        self.header_u32(bp, 8)
    }

    fn set_header_u32(&self, bp: &mut BufferPool, off: usize, v: PageNo) -> Result<()> {
        bp.with_page(self.file, 0, |page| {
            u32_put(page, off, v);
            Ok(())
        })
    }

    pub fn insert(&self, bp: &mut BufferPool, key: &[u8], rid: Rid) -> Result<()> {
        let root = self.root(bp)?;
        let leaf = if root == 0 {
            let no = bp.alloc_page(self.file)?;
            bp.with_page(self.file, no, |page| {
                leaf_init(page, 0, 0);
                Ok(())
            })?;
            self.set_header_u32(bp, 8, no)?;
            self.set_header_u32(bp, 12, no)?;
            no
        } else {
            self.descend(bp, root, key)?
        };
        bp.with_page(self.file, leaf, |page| {
            let idx = leaf_lower_bound(page, key);
            leaf_insert_at(page, idx, key, rid)
        })
    }

    fn descend(&self, bp: &mut BufferPool, mut page_no: PageNo, key: &[u8]) -> Result<PageNo> {
        loop {
            let ty = bp.read_page(self.file, page_no, |page| Ok(node_type(page)))?;
            if ty == LEAF {
                return Ok(page_no);
            }
            if ty != INTERNAL {
                return Err(Error::Runtime(format!("corrupt index page {page_no}")));
            }
            page_no = bp.read_page(self.file, page_no, |page| Ok(internal_child_for(page, key)))?;
        }
    }

    pub fn search(&self, bp: &mut BufferPool, key: &[u8]) -> Result<Vec<Rid>> {
        let root = self.root(bp)?;
        if root == 0 {
            return Ok(vec![]);
        }
        let leaf = self.descend(bp, root, key)?;
        let mut out = Vec::new();
        bp.read_page(self.file, leaf, |page| {
            let mut i = leaf_lower_bound(page, key);
            let n = leaf_num_at(page);
            while i < n {
                let (k, rid) = leaf_entry(page, i);
                if k.as_slice() != key {
                    break;
                }
                out.push(rid);
                i += 1;
            }
            Ok(())
        })?;
        Ok(out)
    }
}

fn u32_get(page: &[u8], off: usize) -> PageNo {
    u32::from_le_bytes(page[off..off + 4].try_into().unwrap())
}

fn u32_put(page: &mut [u8], off: usize, v: PageNo) {
    page[off..off + 4].copy_from_slice(&(v as u32).to_le_bytes());
}

fn leaf_num_at(page: &[u8]) -> usize {
    u16::from_le_bytes([page[1], page[2]]) as usize
}

fn leaf_entry(page: &[u8], i: usize) -> (Vec<u8>, Rid) {
    crate::index::node::leaf_entry_at(page, i)
}
