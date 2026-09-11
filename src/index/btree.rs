use crate::index::node::{
    internal_bytes_used, internal_child_for, internal_entry_at, internal_entries,
    internal_first_child, internal_init, internal_insert_entry, internal_num, internal_remove_at,
    internal_set_first_child, leaf_bytes_used, leaf_entries, leaf_init, leaf_insert_at,
    leaf_lower_bound, leaf_next, leaf_num, leaf_prev, leaf_remove_at, leaf_set_next,
    leaf_set_prev, leaf_upper_bound, node_type, leaf_entry_at, INTERNAL, LEAF,
};
use crate::storage::buffer::BufferPool;
use crate::storage::header::{self, FileKind};
use crate::storage::page::{FileId, PageNo, PAGE_SIZE};
use crate::storage::Rid;
use crate::{Error, Result};

const MAGIC: [u8; 8] = *b"CHIDBITX";
/// Root page number in the file header.
const ROOT_OFF: usize = header::HEADER_LEN;
/// First leaf page number in the file header.
const FIRST_LEAF_OFF: usize = header::HEADER_LEN + 4;

/// (separator key, child page) pairs of an internal node.
type InternalEntries = Vec<(Vec<u8>, PageNo)>;

/// Pages below this occupancy (in bytes) are underfull and trigger borrow/merge.
const MIN_OCCUPANCY: usize = PAGE_SIZE / 4;

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
    /// Unvalidated handle for an already-initialized index file.
    pub fn at(file: FileId) -> Self {
        Self { file }
    }

    pub fn init(bp: &mut BufferPool, file: FileId) -> Result<Self> {
        if bp.page_count(file)? != 0 {
            return Err(Error::Runtime("cannot init index file: file not empty".into()));
        }
        let no = bp.alloc_page(file)?;
        bp.with_page(file, no, |page| {
            header::write_header(page, &MAGIC, FileKind::Index);
            u32_put(page, ROOT_OFF, 0); // root: empty tree
            u32_put(page, FIRST_LEAF_OFF, 0); // first leaf
            Ok(())
        })?;
        Ok(Self { file })
    }

    pub fn open(bp: &mut BufferPool, file: FileId) -> Result<Self> {
        if bp.page_count(file)? == 0 {
            return Err(Error::Runtime("cannot open index file: file is empty".into()));
        }
        bp.read_page(file, 0, |page| header::read_header(page, &MAGIC, FileKind::Index))?;
        Ok(Self { file })
    }

    /// Opens the file, re-initializing a header that a crash lost before it
    /// reached the disk. Returns true when the file was re-initialized; the
    /// caller must then rebuild the tree contents.
    pub fn open_or_repair(bp: &mut BufferPool, file: FileId) -> Result<bool> {
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
            header::write_header(page, &MAGIC, FileKind::Index);
            u32_put(page, ROOT_OFF, 0); // root: empty tree
            u32_put(page, FIRST_LEAF_OFF, 0); // first leaf
            Ok(())
        })?;
        Ok(true)
    }

    pub fn file_id(&self) -> FileId {
        self.file
    }

    fn header_u32(&self, bp: &mut BufferPool, off: usize) -> Result<PageNo> {
        bp.read_page(self.file, 0, |page| Ok(u32_get(page, off)))
    }

    fn root(&self, bp: &mut BufferPool) -> Result<PageNo> {
        self.header_u32(bp, ROOT_OFF)
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
            self.set_header_u32(bp, ROOT_OFF, no)?;
            self.set_header_u32(bp, FIRST_LEAF_OFF, no)?;
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
                self.set_header_u32(bp, ROOT_OFF, new_root)?;
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
                let first = self.header_u32(bp, FIRST_LEAF_OFF)?;
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

    pub fn delete(&self, bp: &mut BufferPool, key: &[u8], rid: Rid) -> Result<()> {
        let root = self.root(bp)?;
        if root == 0 {
            return Ok(());
        }
        let underflow = self.delete_rec(bp, root, key, rid)?;
        if underflow {
            let root_state = bp.read_page(self.file, root, |page| {
                let ty = node_type(page);
                let n = if ty == LEAF { leaf_num(page) } else { internal_num(page) };
                Ok((ty, n, internal_first_child(page)))
            })?;
            let (ty, n, first_child) = root_state;
            if ty == LEAF && n == 0 {
                self.set_header_u32(bp, ROOT_OFF, 0)?;
                self.set_header_u32(bp, FIRST_LEAF_OFF, 0)?;
            } else if ty == INTERNAL && n == 0 {
                self.set_header_u32(bp, ROOT_OFF, first_child)?;
            }
        }
        Ok(())
    }

    fn delete_rec(
        &self,
        bp: &mut BufferPool,
        page_no: PageNo,
        key: &[u8],
        rid: Rid,
    ) -> Result<bool> {
        let ty = bp.read_page(self.file, page_no, |page| Ok(node_type(page)))?;
        if ty == LEAF {
            bp.with_page(self.file, page_no, |page| {
                let mut i = leaf_lower_bound(page, key);
                loop {
                    if i >= leaf_num(page) {
                        return Err(Error::Runtime("no such index entry".into()));
                    }
                    let (k, r) = leaf_entry_at(page, i);
                    if k.as_slice() != key {
                        return Err(Error::Runtime("no such index entry".into()));
                    }
                    if r == rid {
                        leaf_remove_at(page, i)?;
                        break;
                    }
                    i += 1;
                }
                Ok(leaf_bytes_used(page) < MIN_OCCUPANCY)
            })
        } else if ty == INTERNAL {
            let child =
                bp.read_page(self.file, page_no, |page| Ok(internal_child_for(page, key)))?;
            if !self.delete_rec(bp, child, key, rid)? {
                return Ok(false);
            }
            self.fix_child(bp, page_no, child)?;
            bp.read_page(self.file, page_no, |page| {
                Ok(internal_bytes_used(page) < MIN_OCCUPANCY)
            })
        } else {
            Err(Error::Runtime(format!("corrupt index page {page_no}")))
        }
    }

    /// Child of `parent` underflowed after a delete: borrow from siblings or merge.
    fn fix_child(&self, bp: &mut BufferPool, parent: PageNo, child: PageNo) -> Result<()> {
        let (child_ty, idx, keys, children) = self.locate_child(bp, parent, child)?;
        let sep_left = if idx >= 1 { Some(keys[idx - 1].clone()) } else { None };
        let sep_right = if idx < keys.len() { Some(keys[idx].clone()) } else { None };

        if child_ty == LEAF {
            self.fix_leaf_child(bp, parent, child, idx, &children, sep_left, sep_right)
        } else {
            self.fix_internal_child(bp, parent, child, idx, children, sep_left, sep_right)
        }
    }

    #[allow(clippy::type_complexity)]
    fn locate_child(
        &self,
        bp: &mut BufferPool,
        parent: PageNo,
        child: PageNo,
    ) -> Result<(u8, usize, Vec<Vec<u8>>, Vec<PageNo>)> {
        let child_ty = bp.read_page(self.file, child, |page| Ok(node_type(page)))?;
        let (keys, children) = bp.read_page(self.file, parent, |page| {
            let mut keys = Vec::new();
            let mut children = vec![internal_first_child(page)];
            for (k, c) in internal_entries(page) {
                keys.push(k);
                children.push(c);
            }
            Ok((keys, children))
        })?;
        let idx = children.iter().position(|&c| c == child).ok_or_else(|| {
            Error::Runtime(format!("corrupt index: child {child} missing in {parent}"))
        })?;
        Ok((child_ty, idx, keys, children))
    }

    #[allow(clippy::too_many_arguments)]
    fn fix_leaf_child(
        &self,
        bp: &mut BufferPool,
        parent: PageNo,
        child: PageNo,
        idx: usize,
        children: &[PageNo],
        _sep_left: Option<Vec<u8>>,
        _sep_right: Option<Vec<u8>>,
    ) -> Result<()> {
        let left = if idx >= 1 { Some(children[idx - 1]) } else { None };
        let right = if idx + 1 < children.len() { Some(children[idx + 1]) } else { None };

        // borrow while a sibling can spare an entry
        loop {
            let child_used =
                bp.read_page(self.file, child, |page| Ok(leaf_bytes_used(page)))?;
            if child_used >= MIN_OCCUPANCY {
                return Ok(());
            }
            if let Some(left) = left {
                let used = bp.read_page(self.file, left, |page| Ok(leaf_bytes_used(page)))?;
                if used > MIN_OCCUPANCY + 64 {
                    let (k, r) = self.pop_last_leaf_entry(bp, left)?;
                    self.prepend_leaf_entry(bp, child, &k, r)?;
                    self.set_separator_key(bp, parent, idx - 1, k)?;
                    continue;
                }
            }
            if let Some(right) = right {
                let used = bp.read_page(self.file, right, |page| Ok(leaf_bytes_used(page)))?;
                if used > MIN_OCCUPANCY + 64 {
                    let (k, r) = self.take_first_leaf_entry(bp, right)?;
                    self.append_leaf_entry(bp, child, &k, r)?;
                    let (first, _) = self.first_leaf_entry(bp, right)?;
                    self.set_separator_key(bp, parent, idx, first)?;
                    continue;
                }
            }
            break;
        }

        let child_used = bp.read_page(self.file, child, |page| Ok(leaf_bytes_used(page)))?;
        if child_used >= MIN_OCCUPANCY {
            return Ok(());
        }

        // merge: prefer the sibling that fits
        let merge_left = left.filter(|&l| {
            self.can_merge_leaves(bp, l, child).unwrap_or(false)
        });
        if let Some(left) = merge_left {
            let child_next = self.leaf_chain_next(bp, child)?;
            self.append_leaf_entries(bp, left, child)?;
            self.set_leaf_next(bp, left, child_next)?;
            if let Some(next) = child_next
                && next != 0 {
                    self.set_leaf_prev(bp, next, left)?;
                }
            internal_sep_remove(bp, self.file, parent, idx - 1)
        } else if let Some(right) = right {
            if self.can_merge_leaves(bp, child, right)? {
                let right_next = self.leaf_chain_next(bp, right)?;
                self.append_leaf_entries(bp, child, right)?;
                self.set_leaf_next(bp, child, right_next)?;
                if let Some(next) = right_next
                    && next != 0 {
                        self.set_leaf_prev(bp, next, child)?;
                    }
                internal_sep_remove(bp, self.file, parent, idx)
            } else {
                Err(Error::Runtime("index merge overflow".into()))
            }
        } else {
            Ok(()) // no siblings (root leaf): nothing to fix
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn fix_internal_child(
        &self,
        bp: &mut BufferPool,
        parent: PageNo,
        child: PageNo,
        idx: usize,
        children: Vec<PageNo>,
        sep_left: Option<Vec<u8>>,
        sep_right: Option<Vec<u8>>,
    ) -> Result<()> {
        let left = if idx >= 1 { Some(children[idx - 1]) } else { None };
        let right = if idx + 1 < children.len() { Some(children[idx + 1]) } else { None };

        loop {
            let child_used =
                bp.read_page(self.file, child, |page| Ok(internal_bytes_used(page)))?;
            if child_used >= MIN_OCCUPANCY {
                return Ok(());
            }
            if let (Some(left), Some(psep)) = (left, sep_left.clone()) {
                let used = bp.read_page(self.file, left, |page| Ok(internal_bytes_used(page)))?;
                let n = bp.read_page(self.file, left, |page| Ok(internal_num(page)))?;
                if used > MIN_OCCUPANCY + 64 && n > 0 {
                    let (lk, lc) = self.pop_last_separator(bp, left)?;
                    let old_first =
                        bp.read_page(self.file, child, |page| Ok(internal_first_child(page)))?;
                    bp.with_page(self.file, child, |page| {
                        internal_insert_entry(page, 0, &psep, old_first)?;
                        internal_set_first_child(page, lc);
                        Ok(())
                    })?;
                    self.set_separator_key(bp, parent, idx - 1, lk)?;
                    continue;
                }
            }
            if let (Some(right), Some(psep)) = (right, sep_right.clone()) {
                let used = bp.read_page(self.file, right, |page| Ok(internal_bytes_used(page)))?;
                let n = bp.read_page(self.file, right, |page| Ok(internal_num(page)))?;
                if used > MIN_OCCUPANCY + 64 && n > 0 {
                    let (rk, rc) = self.take_first_separator(bp, right)?;
                    let rfirst =
                        bp.read_page(self.file, right, |page| Ok(internal_first_child(page)))?;
                    bp.with_page(self.file, child, |page| {
                        let n = internal_num(page);
                        internal_insert_entry(page, n, &psep, rfirst)
                    })?;
                    self.set_internal_first_child(bp, right, rc)?;
                    self.set_separator_key(bp, parent, idx, rk)?;
                    continue;
                }
            }
            break;
        }

        let child_used = bp.read_page(self.file, child, |page| Ok(internal_bytes_used(page)))?;
        if child_used >= MIN_OCCUPANCY {
            return Ok(());
        }

        let can_merge_left = match left {
            Some(l) => self.can_merge_internals(bp, l, child)?,
            None => false,
        };
        if can_merge_left {
            let left = left.expect("can_merge_left implies left sibling");
            let psep = sep_left.expect("left sibling implies separator");
            let (child_first, child_entries) = self.read_internal(bp, child)?;
            bp.with_page(self.file, left, |page| {
                let n = internal_num(page);
                internal_insert_entry(page, n, &psep, child_first)?;
                for (i, (k, c)) in child_entries.iter().enumerate() {
                    internal_insert_entry(page, n + 1 + i, k, *c)?;
                }
                Ok(())
            })?;
            internal_sep_remove(bp, self.file, parent, idx - 1)
        } else if let Some(right) = right {
            if self.can_merge_internals(bp, child, right)? {
                let psep = sep_right.expect("right sibling implies separator");
                let (rfirst, rentries) = self.read_internal(bp, right)?;
                bp.with_page(self.file, child, |page| {
                    let n = internal_num(page);
                    internal_insert_entry(page, n, &psep, rfirst)?;
                    for (i, (k, c)) in rentries.iter().enumerate() {
                        internal_insert_entry(page, n + 1 + i, k, *c)?;
                    }
                    Ok(())
                })?;
                internal_sep_remove(bp, self.file, parent, idx)
            } else {
                Err(Error::Runtime("index merge overflow".into()))
            }
        } else {
            Ok(())
        }
    }

    fn read_internal(
        &self,
        bp: &mut BufferPool,
        page: PageNo,
    ) -> Result<(PageNo, InternalEntries)> {
        bp.read_page(self.file, page, |p| {
            Ok((internal_first_child(p), internal_entries(p).collect()))
        })
    }

    fn pop_last_separator(&self, bp: &mut BufferPool, page: PageNo) -> Result<(Vec<u8>, PageNo)> {
        let (n, entry) = bp.read_page(self.file, page, |p| {
            Ok((internal_num(p), internal_entry_at(p, internal_num(p) - 1)))
        })?;
        bp.with_page(self.file, page, |p| internal_remove_at(p, n - 1))?;
        Ok(entry)
    }

    fn take_first_separator(
        &self,
        bp: &mut BufferPool,
        page: PageNo,
    ) -> Result<(Vec<u8>, PageNo)> {
        let entry = bp.read_page(self.file, page, |p| Ok(internal_entry_at(p, 0)))?;
        bp.with_page(self.file, page, |p| {
            internal_remove_at(p, 0)?;
            Ok(())
        })?;
        Ok(entry)
    }

    fn set_internal_first_child(&self, bp: &mut BufferPool, page: PageNo, child: PageNo) -> Result<()> {
        bp.with_page(self.file, page, |p| {
            internal_set_first_child(p, child);
            Ok(())
        })
    }

    fn set_separator_key(
        &self,
        bp: &mut BufferPool,
        parent: PageNo,
        sep_idx: usize,
        new_key: Vec<u8>,
    ) -> Result<()> {
        let child = bp.read_page(self.file, parent, |p| {
            Ok(internal_entry_at(p, sep_idx).1)
        })?;
        bp.with_page(self.file, parent, |p| {
            internal_remove_at(p, sep_idx)?;
            internal_insert_entry(p, sep_idx, &new_key, child)
        })
    }

    fn pop_last_leaf_entry(&self, bp: &mut BufferPool, page: PageNo) -> Result<(Vec<u8>, Rid)> {
        let (n, entry) = bp.read_page(self.file, page, |p| {
            Ok((leaf_num(p), leaf_entry_at(p, leaf_num(p) - 1)))
        })?;
        bp.with_page(self.file, page, |p| leaf_remove_at(p, n - 1))?;
        Ok(entry)
    }

    fn take_first_leaf_entry(&self, bp: &mut BufferPool, page: PageNo) -> Result<(Vec<u8>, Rid)> {
        let entry = bp.read_page(self.file, page, |p| Ok(leaf_entry_at(p, 0)))?;
        bp.with_page(self.file, page, |p| leaf_remove_at(p, 0))?;
        Ok(entry)
    }

    fn first_leaf_entry(&self, bp: &mut BufferPool, page: PageNo) -> Result<(Vec<u8>, Rid)> {
        bp.read_page(self.file, page, |p| Ok(leaf_entry_at(p, 0)))
    }

    fn prepend_leaf_entry(&self, bp: &mut BufferPool, page: PageNo, key: &[u8], rid: Rid) -> Result<()> {
        bp.with_page(self.file, page, |p| leaf_insert_at(p, 0, key, rid))
    }

    fn append_leaf_entry(&self, bp: &mut BufferPool, page: PageNo, key: &[u8], rid: Rid) -> Result<()> {
        bp.with_page(self.file, page, |p| {
            let n = leaf_num(p);
            leaf_insert_at(p, n, key, rid)
        })
    }

    fn append_leaf_entries(&self, bp: &mut BufferPool, target: PageNo, source: PageNo) -> Result<()> {
        let entries: Vec<(Vec<u8>, Rid)> =
            bp.read_page(self.file, source, |p| Ok(leaf_entries(p).collect()))?;
        bp.with_page(self.file, target, |p| {
            let n = leaf_num(p);
            for (i, (k, r)) in (n..).zip(entries) {
                leaf_insert_at(p, i, &k, r)?;
            }
            Ok(())
        })
    }

    fn leaf_chain_next(&self, bp: &mut BufferPool, page: PageNo) -> Result<Option<PageNo>> {
        bp.read_page(self.file, page, |p| {
            let next = leaf_next(p);
            Ok(if next == 0 { None } else { Some(next) })
        })
    }

    fn set_leaf_next(&self, bp: &mut BufferPool, page: PageNo, next: Option<PageNo>) -> Result<()> {
        bp.with_page(self.file, page, |p| {
            leaf_set_next(p, next.unwrap_or(0));
            Ok(())
        })
    }

    fn set_leaf_prev(&self, bp: &mut BufferPool, page: PageNo, prev: PageNo) -> Result<()> {
        bp.with_page(self.file, page, |p| {
            leaf_set_prev(p, prev);
            Ok(())
        })
    }

    fn can_merge_leaves(&self, bp: &mut BufferPool, a: PageNo, b: PageNo) -> Result<bool> {
        let ua = bp.read_page(self.file, a, |p| Ok(leaf_bytes_used(p)))?;
        let ub = bp.read_page(self.file, b, |p| Ok(leaf_bytes_used(p)))?;
        Ok(ua + ub < PAGE_SIZE - 32)
    }

    fn can_merge_internals(&self, bp: &mut BufferPool, a: PageNo, b: PageNo) -> Result<bool> {
        let ua = bp.read_page(self.file, a, |p| Ok(internal_bytes_used(p)))?;
        let ub = bp.read_page(self.file, b, |p| Ok(internal_bytes_used(p)))?;
        Ok(ua + ub < PAGE_SIZE - 32)
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
    page[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn leaf_entry(page: &[u8], i: usize) -> (Vec<u8>, Rid) {
    leaf_entry_at(page, i)
}

fn internal_sep_remove(
    bp: &mut BufferPool,
    file: FileId,
    page: PageNo,
    idx: usize,
) -> Result<()> {
    bp.with_page(file, page, |p| internal_remove_at(p, idx))
}
