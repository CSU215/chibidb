use crate::index::node::{
    internal_child_for, internal_entries, internal_first_child, internal_init,
    internal_insert_entry, leaf_entries, leaf_init, leaf_insert_at, leaf_lower_bound, leaf_next,
    leaf_prev, leaf_upper_bound, node_type, INTERNAL, LEAF,
};
use crate::storage::buffer::BufferPool;
use crate::storage::page::{FileId, PageNo};
use crate::storage::Rid;
use crate::{Error, Result};

const MAGIC: [u8; 8] = *b"CHIDBTX1";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Bound<'a> {
    Included(&'a [u8]),
    Excluded(&'a [u8]),
    Unbounded,
}

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

    pub fn height(&self, bp: &mut BufferPool) -> Result<u32> {
        let mut no = self.root(bp)?;
        if no == 0 {
            return Ok(0);
        }
        let mut h = 1;
        loop {
            let ty = bp.read_page(self.file, no, |page| Ok(node_type(page)))?;
            if ty == LEAF {
                return Ok(h);
            }
            no = bp.read_page(self.file, no, |page| Ok(internal_first_child(page)))?;
            h += 1;
        }
    }

    pub fn insert(&self, bp: &mut BufferPool, key: &[u8], rid: Rid) -> Result<()> {
        let root = self.root(bp)?;
        if root == 0 {
            let no = bp.alloc_page(self.file)?;
            bp.with_page(self.file, no, |page| {
                leaf_init(page, 0, 0);
                Ok(())
            })?;
            self.set_header_u32(bp, 8, no)?;
            self.set_header_u32(bp, 12, no)?;
            return self.insert_leaf_entry(bp, no, key, rid).map(|_| ());
        }
        match self.insert_rec(bp, root, key, rid)? {
            None => Ok(()),
            Some((sep, new_child)) => {
                let new_root = bp.alloc_page(self.file)?;
                bp.with_page(self.file, new_root, |page| {
                    internal_init(page, root);
                    internal_insert_entry(page, 0, &sep, new_child)
                })?;
                self.set_header_u32(bp, 8, new_root)?;
                Ok(())
            }
        }
    }

    fn insert_rec(
        &self,
        bp: &mut BufferPool,
        page_no: PageNo,
        key: &[u8],
        rid: Rid,
    ) -> Result<Option<(Vec<u8>, PageNo)>> {
        let ty = bp.read_page(self.file, page_no, |page| Ok(node_type(page)))?;
        if ty == LEAF {
            self.insert_leaf(bp, page_no, key, rid)
        } else if ty == INTERNAL {
            let child =
                bp.read_page(self.file, page_no, |page| Ok(internal_child_for(page, key)))?;
            match self.insert_rec(bp, child, key, rid)? {
                None => Ok(None),
                Some((sep, new_child)) => {
                    self.insert_separator(bp, page_no, child, sep, new_child)
                }
            }
        } else {
            Err(Error::Runtime(format!("corrupt index page {page_no}")))
        }
    }

    fn insert_leaf_entry(
        &self,
        bp: &mut BufferPool,
        page_no: PageNo,
        key: &[u8],
        rid: Rid,
    ) -> Result<()> {
        // upper bound: append after existing equal keys so scan order matches insertion order
        let idx = bp.read_page(self.file, page_no, |page| Ok(leaf_upper_bound(page, key)))?;
        bp.with_page(self.file, page_no, |page| leaf_insert_at(page, idx, key, rid))
    }

    fn insert_leaf(
        &self,
        bp: &mut BufferPool,
        page_no: PageNo,
        key: &[u8],
        rid: Rid,
    ) -> Result<Option<(Vec<u8>, PageNo)>> {
        match self.insert_leaf_entry(bp, page_no, key, rid) {
            Ok(()) => Ok(None),
            Err(Error::PageFull) => {
                let mut all: Vec<(Vec<u8>, Rid)> =
                    bp.read_page(self.file, page_no, |page| Ok(leaf_entries(page).collect()))?;
                let pos = all.partition_point(|(k, _)| k.as_slice() <= key);
                all.insert(pos, (key.to_vec(), rid));

                let old_prev = bp.read_page(self.file, page_no, |page| Ok(leaf_prev(page)))?;
                let old_next = bp.read_page(self.file, page_no, |page| Ok(leaf_next(page)))?;
                let m = all.len() - all.len() / 2;
                let sep = all[m].0.clone();
                let new_no = bp.alloc_page(self.file)?;

                bp.with_page(self.file, page_no, |page| {
                    leaf_init(page, old_prev, new_no);
                    for (i, (k, r)) in all[..m].iter().enumerate() {
                        leaf_insert_at(page, i, k, *r)?;
                    }
                    Ok(())
                })?;
                bp.with_page(self.file, new_no, |page| {
                    leaf_init(page, page_no, old_next);
                    for (i, (k, r)) in all[m..].iter().enumerate() {
                        leaf_insert_at(page, i, k, *r)?;
                    }
                    Ok(())
                })?;
                if old_next != 0 {
                    bp.with_page(self.file, old_next, |page| {
                        crate::index::node::leaf_set_prev(page, new_no);
                        Ok(())
                    })?;
                }
                Ok(Some((sep, new_no)))
            }
            Err(e) => Err(e),
        }
    }

    fn insert_separator(
        &self,
        bp: &mut BufferPool,
        page_no: PageNo,
        child: PageNo,
        sep: Vec<u8>,
        new_child: PageNo,
    ) -> Result<Option<(Vec<u8>, PageNo)>> {
        let idx = bp.read_page(self.file, page_no, |page| {
            if child == internal_first_child(page) {
                return Ok(0usize);
            }
            for (i, (_, c)) in internal_entries(page).enumerate() {
                if c == child {
                    return Ok(i + 1);
                }
            }
            Err(Error::Runtime(format!("corrupt index: child {child} not found")))
        })?;
        let inserted =
            bp.with_page(self.file, page_no, |page| internal_insert_entry(page, idx, &sep, new_child));
        match inserted {
            Ok(()) => Ok(None),
            Err(Error::PageFull) => {
                let mut keys: Vec<Vec<u8>> =
                    bp.read_page(self.file, page_no, |page| {
                        Ok(internal_entries(page).map(|(k, _)| k).collect())
                    })?;
                let mut children: Vec<PageNo> =
                    bp.read_page(self.file, page_no, |page| {
                        let mut v = vec![internal_first_child(page)];
                        v.extend(internal_entries(page).map(|(_, c)| c));
                        Ok(v)
                    })?;
                keys.insert(idx, sep);
                children.insert(idx + 1, new_child);

                let k = keys.len();
                let m = k / 2;
                let promoted = keys[m].clone();
                let new_no = bp.alloc_page(self.file)?;

                bp.with_page(self.file, page_no, |page| {
                    internal_init(page, children[0]);
                    for i in 0..m {
                        internal_insert_entry(page, i, &keys[i], children[i + 1])?;
                    }
                    Ok(())
                })?;
                bp.with_page(self.file, new_no, |page| {
                    internal_init(page, children[m + 1]);
                    for i in (m + 1)..k {
                        internal_insert_entry(page, i - m - 1, &keys[i], children[i + 1])?;
                    }
                    Ok(())
                })?;
                Ok(Some((promoted, new_no)))
            }
            Err(e) => Err(e),
        }
    }

    pub fn scan_range(
        &self,
        bp: &mut BufferPool,
        start: Bound,
        end: Bound,
    ) -> Result<Vec<(Vec<u8>, Rid)>> {
        let root = self.root(bp)?;
        if root == 0 {
            return Ok(vec![]);
        }
        let (mut page_no, mut pos) = match start {
            Bound::Unbounded => {
                let first = self.header_u32(bp, 12)?;
                if first == 0 {
                    return Ok(vec![]);
                }
                (first, 0)
            }
            Bound::Included(key) | Bound::Excluded(key) => {
                let leaf = self.descend(bp, root, key)?;
                // walk back to the first leaf holding this key (duplicate run)
                let mut leaf = leaf;
                loop {
                    let back = bp.read_page(self.file, leaf, |page| {
                        let prev = leaf_prev(page);
                        if prev == 0 || leaf_num(page) == 0 {
                            return Ok(None);
                        }
                        let (first_key, _) = leaf_entry(page, 0);
                        if first_key.as_slice() == key {
                            Ok(Some(prev))
                        } else {
                            Ok(None)
                        }
                    })?;
                    match back {
                        Some(prev) => leaf = prev,
                        None => break,
                    }
                }
                (leaf, bp.read_page(self.file, leaf, |page| Ok(leaf_lower_bound(page, key)))?)
            }
        };

        let mut out = Vec::new();
        loop {
            let next = bp.read_page(self.file, page_no, |page| {
                while pos < leaf_num(page) {
                    let (k, rid) = leaf_entry(page, pos);
                    pos += 1;
                    let after_start = match start {
                        Bound::Unbounded => true,
                        Bound::Included(s) => k.as_slice() >= s,
                        Bound::Excluded(s) => k.as_slice() > s,
                    };
                    if !after_start {
                        continue;
                    }
                    let past_end = match end {
                        Bound::Unbounded => false,
                        Bound::Included(e) => k.as_slice() > e,
                        Bound::Excluded(e) => k.as_slice() >= e,
                    };
                    if past_end {
                        return Ok(None);
                    }
                    out.push((k, rid));
                }
                Ok(Some(leaf_next(page)))
            })?;
            match next {
                Some(next) if next != 0 => {
                    page_no = next;
                    pos = 0;
                }
                _ => return Ok(out),
            }
        }
    }

    pub fn search(&self, bp: &mut BufferPool, key: &[u8]) -> Result<Vec<Rid>> {
        let root = self.root(bp)?;
        if root == 0 {
            return Ok(vec![]);
        }
        let mut page_no = self.descend(bp, root, key)?;
        // walk back to the first leaf holding this key (duplicates span leaves)
        loop {
            let back = bp.read_page(self.file, page_no, |page| {
                let prev = leaf_prev(page);
                if prev == 0 || leaf_num(page) == 0 {
                    return Ok(None);
                }
                let (first_key, _) = leaf_entry(page, 0);
                if first_key.as_slice() == key {
                    Ok(Some(prev))
                } else {
                    Ok(None)
                }
            })?;
            match back {
                Some(prev) => page_no = prev,
                None => break,
            }
        }
        let mut out = Vec::new();
        loop {
            let next = bp.read_page(self.file, page_no, |page| {
                let mut i = leaf_lower_bound(page, key);
                while i < leaf_num(page) {
                    let (k, rid) = leaf_entry(page, i);
                    if k.as_slice() != key {
                        return Ok(None); // exhausted equal run: stop entirely
                    }
                    out.push(rid);
                    i += 1;
                }
                Ok(Some(leaf_next(page))) // leaf exhausted: follow the chain
            })?;
            match next {
                Some(next) if next != 0 => page_no = next,
                _ => return Ok(out),
            }
        }
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
}

fn u32_get(page: &[u8], off: usize) -> PageNo {
    u32::from_le_bytes(page[off..off + 4].try_into().unwrap())
}

fn u32_put(page: &mut [u8], off: usize, v: PageNo) {
    page[off..off + 4].copy_from_slice(&(v as u32).to_le_bytes());
}

fn leaf_num(page: &[u8]) -> usize {
    u16::from_le_bytes([page[1], page[2]]) as usize
}

fn leaf_entry(page: &[u8], i: usize) -> (Vec<u8>, Rid) {
    crate::index::node::leaf_entry_at(page, i)
}
