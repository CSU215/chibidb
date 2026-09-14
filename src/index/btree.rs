use crate::index::node::{
    internal_bytes_used, internal_child_for, internal_entry_at, internal_entry_size,
    internal_entries, internal_first_child, internal_header_len, internal_high_key, internal_init,
    internal_insert_entry, internal_next, internal_num, internal_remove_at, internal_set_first_child,
    internal_set_high_key, internal_set_next, leaf_bytes_used, leaf_entries, leaf_entry_size,
    leaf_header_len, leaf_high_key, leaf_init, leaf_insert_at, leaf_lower_bound, leaf_next, leaf_num,
    leaf_prev, leaf_remove_at, leaf_set_high_key, leaf_set_next, leaf_set_prev, leaf_upper_bound,
    node_type, leaf_entry_at, INTERNAL, LEAF,
};
use crate::storage::buffer::BufferPool;
use crate::storage::header::{self, FileKind};
use crate::storage::page::{zeroed_page, FileId, PageData, PageNo, PAGE_SIZE};
use crate::storage::Rid;
use crate::{Error, Result};

const MAGIC: [u8; 8] = *b"CHIDBITZ";
/// Root page number in the file header.
const ROOT_OFF: usize = header::HEADER_LEN;
/// First leaf page number in the file header.
const FIRST_LEAF_OFF: usize = header::HEADER_LEN + 4;

/// (separator key, child page) pairs of an internal node.
type InternalEntries = Vec<(Vec<u8>, PageNo)>;

/// (smallest key, largest key) of a subtree; both `None` when it is empty.
type KeyRange = (Option<Vec<u8>>, Option<Vec<u8>>);

/// Pages below this occupancy (in bytes) are underfull and trigger borrow/merge.
const MIN_OCCUPANCY: usize = PAGE_SIZE / 4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Bound<'a> {
    Included(&'a [u8]),
    Excluded(&'a [u8]),
    Unbounded,
}

/// Outcome of a recursive delete.
enum DeleteOutcome {
    /// The `(key, rid)` pair is not present in this subtree.
    NotFound,
    /// The pair was removed; `underflow` marks the page as below `MIN_OCCUPANCY`.
    Deleted { underflow: bool },
}

/// Builds a leaf page image from sorted entries, or `None` if they do not fit.
fn build_leaf(
    entries: &[(Vec<u8>, Rid)],
    prev: PageNo,
    next: PageNo,
    high_key: &[u8],
) -> Option<PageData> {
    let mut page = zeroed_page();
    leaf_init(&mut page, prev, next);
    if leaf_set_high_key(&mut page, high_key).is_err() {
        return None;
    }
    for (i, (key, rid)) in entries.iter().enumerate() {
        if leaf_insert_at(&mut page, i, key, *rid).is_err() {
            return None;
        }
    }
    Some(page)
}

/// Builds an internal page image from `first_child` and sorted separators, or
/// `None` if they do not fit.
fn build_internal(
    first_child: PageNo,
    entries: &[(Vec<u8>, PageNo)],
    next: PageNo,
    high_key: &[u8],
) -> Option<PageData> {
    let mut page = zeroed_page();
    internal_init(&mut page, first_child);
    internal_set_next(&mut page, next);
    if internal_set_high_key(&mut page, high_key).is_err() {
        return None;
    }
    for (i, (key, child)) in entries.iter().enumerate() {
        if internal_insert_entry(&mut page, i, key, *child).is_err() {
            return None;
        }
    }
    Some(page)
}

/// Replaces a page's contents with a fully-built image in one write, so a
/// failed build never leaves a partially rewritten page.
fn write_image(bp: &BufferPool, file: FileId, page_no: PageNo, image: &PageData) -> Result<()> {
    bp.with_page(file, page_no, |page| {
        page.copy_from_slice(&image[..]);
        Ok(())
    })
}

/// Picks a leaf split point `m`: left `[..m]` carries high key `entries[m].0`,
/// right `[m..]` keeps the old high key. Both halves must fit a page.
fn choose_leaf_split(all: &[(Vec<u8>, Rid)], old_hk_len: usize) -> Option<usize> {
    let sizes: Vec<usize> = all.iter().map(|(k, _)| leaf_entry_size(k.len())).collect();
    choose_split_with(&sizes, |m| leaf_header_len(all[m].0.len()), |_| {
        leaf_header_len(old_hk_len)
    })
}

/// Picks an internal split point `m` (promoted separator `keys[m]`): left keeps
/// `keys[m]` as its high key, right keeps the old one.
fn choose_internal_split(keys: &[Vec<u8>], old_hk_len: usize) -> Option<usize> {
    let sizes: Vec<usize> = keys.iter().map(|k| internal_entry_size(k.len())).collect();
    let n = sizes.len();
    let mut prefix = vec![0usize; n + 1];
    for i in 0..n {
        prefix[i + 1] = prefix[i] + sizes[i];
    }
    let total = prefix[n];
    let mut best: Option<(usize, usize)> = None;
    for (m, window) in prefix.windows(2).enumerate().skip(1) {
        let left = internal_header_len(keys[m].len()) + window[0];
        let right = internal_header_len(old_hk_len) + (total - window[1]);
        if left <= PAGE_SIZE && right <= PAGE_SIZE {
            let diff = left.abs_diff(right);
            if best.is_none_or(|(bd, _)| diff < bd) {
                best = Some((diff, m));
            }
        }
    }
    best.map(|(_, m)| m)
}

/// Shared prefix-sum split choice; the two headers may depend on the split.
fn choose_split_with(
    sizes: &[usize],
    left_header: impl Fn(usize) -> usize,
    right_header: impl Fn(usize) -> usize,
) -> Option<usize> {
    let n = sizes.len();
    let mut prefix = vec![0usize; n + 1];
    for i in 0..n {
        prefix[i + 1] = prefix[i] + sizes[i];
    }
    let total = prefix[n];
    let mut best: Option<(usize, usize)> = None;
    for (m, window) in prefix.windows(2).enumerate().skip(1) {
        let left = left_header(m) + window[0];
        let right = right_header(m) + (total - window[0]);
        if left <= PAGE_SIZE && right <= PAGE_SIZE {
            let diff = left.abs_diff(right);
            if best.is_none_or(|(bd, _)| diff < bd) {
                best = Some((diff, m));
            }
        }
    }
    best.map(|(_, m)| m)
}

/// Children of an internal node that may contain `key`, leftmost first. A run
/// of separators equal to `key` can leave equal keys on several adjacent
/// children, so all of them are candidates.
fn delete_candidates(page: &[u8], key: &[u8]) -> Vec<(usize, PageNo)> {
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut children = vec![internal_first_child(page)];
    for (k, c) in internal_entries(page) {
        keys.push(k);
        children.push(c);
    }
    let lo = keys.partition_point(|sep| sep.as_slice() < key);
    let hi = keys.partition_point(|sep| sep.as_slice() <= key);
    (lo..=hi).map(|idx| (idx, children[idx])).collect()
}

pub struct BTree {
    file: FileId,
}

impl BTree {
    /// Unvalidated handle for an already-initialized index file.
    pub fn at(file: FileId) -> Self {
        Self { file }
    }

    pub fn init(bp: &BufferPool, file: FileId) -> Result<Self> {
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

    pub fn open(bp: &BufferPool, file: FileId) -> Result<Self> {
        if bp.page_count(file)? == 0 {
            return Err(Error::Runtime("cannot open index file: file is empty".into()));
        }
        bp.read_page(file, 0, |page| header::read_header(page, &MAGIC, FileKind::Index))?;
        Ok(Self { file })
    }

    /// Opens the file, re-initializing a header that a crash lost before it
    /// reached the disk. Returns true when the file was re-initialized; the
    /// caller must then rebuild the tree contents.
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

    fn header_u32(&self, bp: &BufferPool, off: usize) -> Result<PageNo> {
        bp.read_page(self.file, 0, |page| Ok(u32_get(page, off)))
    }

    fn root(&self, bp: &BufferPool) -> Result<PageNo> {
        self.header_u32(bp, ROOT_OFF)
    }

    fn set_header_u32(&self, bp: &BufferPool, off: usize, v: PageNo) -> Result<()> {
        bp.with_page(self.file, 0, |page| {
            u32_put(page, off, v);
            Ok(())
        })
    }

    pub fn height(&self, bp: &BufferPool) -> Result<u32> {
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

    pub fn insert(&self, bp: &BufferPool, key: &[u8], rid: Rid) -> Result<()> {
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
        bp: &BufferPool,
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
        bp: &BufferPool,
        page_no: PageNo,
        key: &[u8],
        rid: Rid,
    ) -> Result<()> {
        // upper bound: append after existing equal keys so scan order matches insertion order
        let idx = bp.read_page(self.file, page_no, |page| Ok(leaf_upper_bound(page, key)))?;
        bp.with_page(self.file, page_no, |page| match leaf_insert_at(page, idx, key, rid) {
            Ok(()) => Ok(()),
            // an empty leaf that cannot hold the key means it never will
            Err(Error::PageFull) if leaf_num(page) == 0 => {
                Err(Error::Runtime("index key too large".into()))
            }
            Err(e) => Err(e),
        })
    }

    fn insert_leaf(
        &self,
        bp: &BufferPool,
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
                let old_hk = bp.read_page(self.file, page_no, |page| Ok(leaf_high_key(page)))?;
                let Some(m) = choose_leaf_split(&all, old_hk.len()) else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                let sep = all[m].0.clone();
                let new_no = bp.alloc_page(self.file)?;
                // Build both halves in scratch first, so a page is never left
                // partially rewritten if a half does not fit. The left half's
                // B-link points at the new right half; the right half keeps the
                // old sibling and high key.
                let Some(left) = build_leaf(&all[..m], old_prev, new_no, &sep) else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                let Some(right) = build_leaf(&all[m..], page_no, old_next, &old_hk) else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                write_image(bp, self.file, page_no, &left)?;
                write_image(bp, self.file, new_no, &right)?;
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
        bp: &BufferPool,
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
        let inserted = bp.with_page(self.file, page_no, |page| {
            match internal_insert_entry(page, idx, &sep, new_child) {
                Ok(()) => Ok(()),
                Err(Error::PageFull) if internal_num(page) == 0 => {
                    Err(Error::Runtime("index key too large".into()))
                }
                Err(e) => Err(e),
            }
        });
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
                let old_hk =
                    bp.read_page(self.file, page_no, |page| Ok(internal_high_key(page)))?;
                let old_next = bp.read_page(self.file, page_no, |page| Ok(internal_next(page)))?;
                let Some(m) = choose_internal_split(&keys, old_hk.len()) else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                let promoted = keys[m].clone();
                let new_no = bp.alloc_page(self.file)?;
                // Build both halves in scratch, then swap them in: the left
                // half links to the new right half, which keeps the old sibling.
                let left_entries: Vec<(Vec<u8>, PageNo)> =
                    (0..m).map(|i| (keys[i].clone(), children[i + 1])).collect();
                let right_entries: Vec<(Vec<u8>, PageNo)> =
                    ((m + 1)..k).map(|i| (keys[i].clone(), children[i + 1])).collect();
                let Some(left) = build_internal(children[0], &left_entries, new_no, &promoted) else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                let Some(right) =
                    build_internal(children[m + 1], &right_entries, old_next, &old_hk)
                else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                write_image(bp, self.file, page_no, &left)?;
                write_image(bp, self.file, new_no, &right)?;
                Ok(Some((promoted, new_no)))
            }
            Err(e) => Err(e),
        }
    }

    pub fn scan_range(
        &self,
        bp: &BufferPool,
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

    /// A forward cursor over the leaf level, so a range can be read lazily in
    /// key order (e.g. an `ORDER BY` that an index can satisfy, stopping after
    /// the caller's LIMIT instead of materializing the whole range).
    pub fn leaf_cursor(&self, bp: &BufferPool) -> Result<LeafCursor> {
        let first = self.header_u32(bp, FIRST_LEAF_OFF)?;
        Ok(LeafCursor { file: self.file, leaf: first, pos: 0 })
    }

    pub fn delete(&self, bp: &BufferPool, key: &[u8], rid: Rid) -> Result<()> {
        let root = self.root(bp)?;
        if root == 0 {
            return Ok(());
        }
        match self.delete_rec(bp, root, key, rid)? {
            DeleteOutcome::NotFound => Err(Error::Runtime("no such index entry".into())),
            DeleteOutcome::Deleted { underflow } => {
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
                        // the child is now the root: unbounded, no right sibling
                        let child_ty = bp.read_page(self.file, first_child, |p| Ok(node_type(p)))?;
                        if child_ty == LEAF {
                            self.set_leaf_high_key(bp, first_child, &[])?;
                            self.set_leaf_next(bp, first_child, None)?;
                        } else {
                            self.set_internal_high_key(bp, first_child, &[])?;
                            bp.with_page(self.file, first_child, |p| {
                                internal_set_next(p, 0);
                                Ok(())
                            })?;
                        }
                    }
                }
                Ok(())
            }
        }
    }

    fn delete_rec(
        &self,
        bp: &BufferPool,
        page_no: PageNo,
        key: &[u8],
        rid: Rid,
    ) -> Result<DeleteOutcome> {
        let ty = bp.read_page(self.file, page_no, |page| Ok(node_type(page)))?;
        if ty == LEAF {
            bp.with_page(self.file, page_no, |page| {
                let mut i = leaf_lower_bound(page, key);
                while i < leaf_num(page) {
                    let (k, r) = leaf_entry_at(page, i);
                    if k.as_slice() != key {
                        return Ok(DeleteOutcome::NotFound);
                    }
                    if r == rid {
                        leaf_remove_at(page, i)?;
                        return Ok(DeleteOutcome::Deleted {
                            underflow: leaf_bytes_used(page) < MIN_OCCUPANCY,
                        });
                    }
                    i += 1;
                }
                Ok(DeleteOutcome::NotFound)
            })
        } else if ty == INTERNAL {
            // A split may leave keys equal to a separator on both sides, so try
            // the left child of an equal separator first, then the right.
            let candidates =
                bp.read_page(self.file, page_no, |page| Ok(delete_candidates(page, key)))?;
            for (child_idx, child) in candidates {
                match self.delete_rec(bp, child, key, rid)? {
                    DeleteOutcome::NotFound => continue,
                    DeleteOutcome::Deleted { underflow } => {
                        if underflow {
                            self.fix_child(bp, page_no, child)?;
                        } else {
                            // the child's first key may have changed: keep the
                            // separator on its left equal to the child minimum
                            self.refresh_separator(bp, page_no, child_idx)?;
                        }
                        let parent_underflow = bp.read_page(self.file, page_no, |page| {
                            Ok(internal_bytes_used(page) < MIN_OCCUPANCY)
                        })?;
                        return Ok(DeleteOutcome::Deleted { underflow: parent_underflow });
                    }
                }
            }
            Ok(DeleteOutcome::NotFound)
        } else {
            Err(Error::Runtime(format!("corrupt index page {page_no}")))
        }
    }

    /// Ensures `keys[child_idx - 1]` equals the minimum key of the child at
    /// `child_idx` (the separator convention after a split).
    fn refresh_separator(&self, bp: &BufferPool, parent: PageNo, child_idx: usize) -> Result<()> {
        if child_idx == 0 {
            return Ok(());
        }
        let child = bp.read_page(self.file, parent, |page| {
            let mut children = vec![internal_first_child(page)];
            children.extend(internal_entries(page).map(|(_, c)| c));
            Ok(children.get(child_idx).copied())
        })?;
        let Some(child) = child else {
            return Ok(());
        };
        if let Some(min) = self.subtree_first_key(bp, child)? {
            self.set_separator_key(bp, parent, child_idx - 1, min)?;
        }
        Ok(())
    }

    /// The smallest key in a page's subtree (its leftmost leaf's first key).
    fn subtree_first_key(&self, bp: &BufferPool, start: PageNo) -> Result<Option<Vec<u8>>> {
        let mut page = start;
        loop {
            let (ty, first_child) = bp.read_page(self.file, page, |p| {
                Ok((node_type(p), internal_first_child(p)))
            })?;
            if ty == LEAF {
                return bp.read_page(self.file, page, |p| {
                    Ok(if leaf_num(p) == 0 {
                        None
                    } else {
                        Some(leaf_entry_at(p, 0).0)
                    })
                });
            }
            if ty != INTERNAL {
                return Err(Error::Runtime(format!("corrupt index page {page}")));
            }
            page = first_child;
        }
    }

    /// Child of `parent` underflowed after a delete: borrow from siblings or merge.
    fn fix_child(&self, bp: &BufferPool, parent: PageNo, child: PageNo) -> Result<()> {
        let (child_ty, idx, children) = self.locate_child(bp, parent, child)?;
        if child_ty == LEAF {
            self.fix_leaf_child(bp, parent, child, idx, &children)
        } else {
            self.fix_internal_child(bp, parent, child, idx, children)
        }
    }

    fn locate_child(
        &self,
        bp: &BufferPool,
        parent: PageNo,
        child: PageNo,
    ) -> Result<(u8, usize, Vec<PageNo>)> {
        let child_ty = bp.read_page(self.file, child, |page| Ok(node_type(page)))?;
        let children = bp.read_page(self.file, parent, |page| {
            let mut children = vec![internal_first_child(page)];
            children.extend(internal_entries(page).map(|(_, c)| c));
            Ok(children)
        })?;
        let idx = children.iter().position(|&c| c == child).ok_or_else(|| {
            Error::Runtime(format!("corrupt index: child {child} missing in {parent}"))
        })?;
        Ok((child_ty, idx, children))
    }

    fn fix_leaf_child(
        &self,
        bp: &BufferPool,
        parent: PageNo,
        child: PageNo,
        idx: usize,
        children: &[PageNo],
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
                    let (k, _) = self.last_leaf_entry(bp, left)?;
                    // only borrow if the entry actually fits the receiver
                    if child_used + leaf_entry_size(k.len()) <= PAGE_SIZE {
                        let (k, r) = self.pop_last_leaf_entry(bp, left)?;
                        self.prepend_leaf_entry(bp, child, &k, r)?;
                        // the moved key is now the child's minimum, i.e. the
                        // left sibling's high key
                        self.set_leaf_high_key(bp, left, &k)?;
                        self.set_separator_key(bp, parent, idx - 1, k)?;
                        continue;
                    }
                }
            }
            if let Some(right) = right {
                let used = bp.read_page(self.file, right, |page| Ok(leaf_bytes_used(page)))?;
                if used > MIN_OCCUPANCY + 64 {
                    let (k, _) = self.first_leaf_entry(bp, right)?;
                    if child_used + leaf_entry_size(k.len()) <= PAGE_SIZE {
                        let (k, r) = self.take_first_leaf_entry(bp, right)?;
                        self.append_leaf_entry(bp, child, &k, r)?;
                        let (first, _) = self.first_leaf_entry(bp, right)?;
                        // the child now covers up to the right sibling's new min
                        self.set_leaf_high_key(bp, child, &first)?;
                        self.set_separator_key(bp, parent, idx, first)?;
                        continue;
                    }
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
            let child_hk = bp.read_page(self.file, child, |p| Ok(leaf_high_key(p)))?;
            self.append_leaf_entries(bp, left, child)?;
            self.set_leaf_next(bp, left, child_next)?;
            // the merged leaf inherits the removed sibling's high key
            self.set_leaf_high_key(bp, left, &child_hk)?;
            if let Some(next) = child_next
                && next != 0 {
                    self.set_leaf_prev(bp, next, left)?;
                }
            internal_sep_remove(bp, self.file, parent, idx - 1)
        } else if let Some(right) = right {
            if self.can_merge_leaves(bp, child, right)? {
                let right_next = self.leaf_chain_next(bp, right)?;
                let right_hk = bp.read_page(self.file, right, |p| Ok(leaf_high_key(p)))?;
                self.append_leaf_entries(bp, child, right)?;
                self.set_leaf_next(bp, child, right_next)?;
                self.set_leaf_high_key(bp, child, &right_hk)?;
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

    fn fix_internal_child(
        &self,
        bp: &BufferPool,
        parent: PageNo,
        child: PageNo,
        idx: usize,
        children: Vec<PageNo>,
    ) -> Result<()> {
        let left = if idx >= 1 { Some(children[idx - 1]) } else { None };
        let right = if idx + 1 < children.len() { Some(children[idx + 1]) } else { None };

        loop {
            let child_used =
                bp.read_page(self.file, child, |page| Ok(internal_bytes_used(page)))?;
            if child_used >= MIN_OCCUPANCY {
                return Ok(());
            }
            if let Some(left) = left {
                let used = bp.read_page(self.file, left, |page| Ok(internal_bytes_used(page)))?;
                let n = bp.read_page(self.file, left, |page| Ok(internal_num(page)))?;
                if used > MIN_OCCUPANCY + 64 && n > 0 {
                    // re-read the current separator: an earlier borrow may have
                    // rotated it
                    let psep = self.separator_key(bp, parent, idx - 1)?;
                    if child_used + internal_entry_size(psep.len()) <= PAGE_SIZE {
                        let (lk, lc) = self.pop_last_separator(bp, left)?;
                        let old_first =
                            bp.read_page(self.file, child, |page| Ok(internal_first_child(page)))?;
                        bp.with_page(self.file, child, |page| {
                            internal_insert_entry(page, 0, &psep, old_first)?;
                            internal_set_first_child(page, lc);
                            Ok(())
                        })?;
                        // the moved separator is now the left sibling's high key
                        self.set_internal_high_key(bp, left, &lk)?;
                        self.set_separator_key(bp, parent, idx - 1, lk)?;
                        continue;
                    }
                }
            }
            if let Some(right) = right {
                let used = bp.read_page(self.file, right, |page| Ok(internal_bytes_used(page)))?;
                let n = bp.read_page(self.file, right, |page| Ok(internal_num(page)))?;
                if used > MIN_OCCUPANCY + 64 && n > 0 {
                    let psep = self.separator_key(bp, parent, idx)?;
                    if child_used + internal_entry_size(psep.len()) <= PAGE_SIZE {
                        let (rk, rc) = self.take_first_separator(bp, right)?;
                        let rfirst =
                            bp.read_page(self.file, right, |page| Ok(internal_first_child(page)))?;
                        bp.with_page(self.file, child, |page| {
                            let n = internal_num(page);
                            internal_insert_entry(page, n, &psep, rfirst)
                        })?;
                        self.set_internal_first_child(bp, right, rc)?;
                        // the child now covers up to the right sibling's new min
                        self.set_internal_high_key(bp, child, &rk)?;
                        self.set_separator_key(bp, parent, idx, rk)?;
                        continue;
                    }
                }
            }
            break;
        }

        let child_used = bp.read_page(self.file, child, |page| Ok(internal_bytes_used(page)))?;
        if child_used >= MIN_OCCUPANCY {
            return Ok(());
        }

        // merge: build the combined page in scratch, then swap it in. The
        // parent separator counts against the page capacity.
        if let Some(left) = left {
            let psep = self.separator_key(bp, parent, idx - 1)?;
            if self.can_merge_internals(bp, left, child, &psep)? {
                let (lfirst, mut entries) = self.read_internal(bp, left)?;
                let (child_first, child_entries) = self.read_internal(bp, child)?;
                let (child_next, child_hk) = bp
                    .read_page(self.file, child, |p| Ok((internal_next(p), internal_high_key(p))))?;
                entries.push((psep, child_first));
                entries.extend(child_entries);
                let Some(image) = build_internal(lfirst, &entries, child_next, &child_hk) else {
                    return Err(Error::Runtime("index merge overflow".into()));
                };
                write_image(bp, self.file, left, &image)?;
                return internal_sep_remove(bp, self.file, parent, idx - 1);
            }
        }
        if let Some(right) = right {
            let psep = self.separator_key(bp, parent, idx)?;
            if self.can_merge_internals(bp, child, right, &psep)? {
                let (cfirst, mut entries) = self.read_internal(bp, child)?;
                let (rfirst, rentries) = self.read_internal(bp, right)?;
                let (right_next, right_hk) = bp
                    .read_page(self.file, right, |p| Ok((internal_next(p), internal_high_key(p))))?;
                entries.push((psep, rfirst));
                entries.extend(rentries);
                let Some(image) = build_internal(cfirst, &entries, right_next, &right_hk) else {
                    return Err(Error::Runtime("index merge overflow".into()));
                };
                write_image(bp, self.file, child, &image)?;
                return internal_sep_remove(bp, self.file, parent, idx);
            }
            return Err(Error::Runtime("index merge overflow".into()));
        }
        Ok(())
    }

    fn read_internal(
        &self,
        bp: &BufferPool,
        page: PageNo,
    ) -> Result<(PageNo, InternalEntries)> {
        bp.read_page(self.file, page, |p| {
            Ok((internal_first_child(p), internal_entries(p).collect()))
        })
    }

    fn pop_last_separator(&self, bp: &BufferPool, page: PageNo) -> Result<(Vec<u8>, PageNo)> {
        let (n, entry) = bp.read_page(self.file, page, |p| {
            Ok((internal_num(p), internal_entry_at(p, internal_num(p) - 1)))
        })?;
        bp.with_page(self.file, page, |p| internal_remove_at(p, n - 1))?;
        Ok(entry)
    }

    fn take_first_separator(
        &self,
        bp: &BufferPool,
        page: PageNo,
    ) -> Result<(Vec<u8>, PageNo)> {
        let entry = bp.read_page(self.file, page, |p| Ok(internal_entry_at(p, 0)))?;
        bp.with_page(self.file, page, |p| {
            internal_remove_at(p, 0)?;
            Ok(())
        })?;
        Ok(entry)
    }

    fn set_internal_first_child(&self, bp: &BufferPool, page: PageNo, child: PageNo) -> Result<()> {
        bp.with_page(self.file, page, |p| {
            internal_set_first_child(p, child);
            Ok(())
        })
    }

    /// The key of separator `sep_idx` in `parent`.
    fn separator_key(&self, bp: &BufferPool, parent: PageNo, sep_idx: usize) -> Result<Vec<u8>> {
        bp.read_page(self.file, parent, |p| Ok(internal_entry_at(p, sep_idx).0))
    }

    /// Replaces one separator key by rebuilding the page in scratch and
    /// swapping it in, so a failed build never mutates the live page.
    fn set_separator_key(
        &self,
        bp: &BufferPool,
        parent: PageNo,
        sep_idx: usize,
        new_key: Vec<u8>,
    ) -> Result<()> {
        let (first_child, mut entries) = self.read_internal(bp, parent)?;
        if sep_idx >= entries.len() {
            return Err(Error::Runtime("corrupt index: separator out of range".into()));
        }
        let (next, hk) =
            bp.read_page(self.file, parent, |p| Ok((internal_next(p), internal_high_key(p))))?;
        entries[sep_idx].0 = new_key;
        let Some(image) = build_internal(first_child, &entries, next, &hk) else {
            return Err(Error::Runtime("index separator too large".into()));
        };
        write_image(bp, self.file, parent, &image)
    }

    fn pop_last_leaf_entry(&self, bp: &BufferPool, page: PageNo) -> Result<(Vec<u8>, Rid)> {
        let (n, entry) = bp.read_page(self.file, page, |p| {
            Ok((leaf_num(p), leaf_entry_at(p, leaf_num(p) - 1)))
        })?;
        bp.with_page(self.file, page, |p| leaf_remove_at(p, n - 1))?;
        Ok(entry)
    }

    fn take_first_leaf_entry(&self, bp: &BufferPool, page: PageNo) -> Result<(Vec<u8>, Rid)> {
        let entry = bp.read_page(self.file, page, |p| Ok(leaf_entry_at(p, 0)))?;
        bp.with_page(self.file, page, |p| leaf_remove_at(p, 0))?;
        Ok(entry)
    }

    fn first_leaf_entry(&self, bp: &BufferPool, page: PageNo) -> Result<(Vec<u8>, Rid)> {
        bp.read_page(self.file, page, |p| Ok(leaf_entry_at(p, 0)))
    }

    fn last_leaf_entry(&self, bp: &BufferPool, page: PageNo) -> Result<(Vec<u8>, Rid)> {
        bp.read_page(self.file, page, |p| Ok(leaf_entry_at(p, leaf_num(p) - 1)))
    }

    fn prepend_leaf_entry(&self, bp: &BufferPool, page: PageNo, key: &[u8], rid: Rid) -> Result<()> {
        bp.with_page(self.file, page, |p| leaf_insert_at(p, 0, key, rid))
    }

    fn append_leaf_entry(&self, bp: &BufferPool, page: PageNo, key: &[u8], rid: Rid) -> Result<()> {
        bp.with_page(self.file, page, |p| {
            let n = leaf_num(p);
            leaf_insert_at(p, n, key, rid)
        })
    }

    fn append_leaf_entries(&self, bp: &BufferPool, target: PageNo, source: PageNo) -> Result<()> {
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

    fn leaf_chain_next(&self, bp: &BufferPool, page: PageNo) -> Result<Option<PageNo>> {
        bp.read_page(self.file, page, |p| {
            let next = leaf_next(p);
            Ok(if next == 0 { None } else { Some(next) })
        })
    }

    fn set_leaf_next(&self, bp: &BufferPool, page: PageNo, next: Option<PageNo>) -> Result<()> {
        bp.with_page(self.file, page, |p| {
            leaf_set_next(p, next.unwrap_or(0));
            Ok(())
        })
    }

    fn set_leaf_prev(&self, bp: &BufferPool, page: PageNo, prev: PageNo) -> Result<()> {
        bp.with_page(self.file, page, |p| {
            leaf_set_prev(p, prev);
            Ok(())
        })
    }

    /// Rewrites a leaf's high key. The header is variable length, so the page is
    /// rebuilt rather than patched in place.
    fn set_leaf_high_key(&self, bp: &BufferPool, page: PageNo, hk: &[u8]) -> Result<()> {
        let (prev, next, entries) = bp.read_page(self.file, page, |p| {
            Ok((leaf_prev(p), leaf_next(p), leaf_entries(p).collect::<Vec<_>>()))
        })?;
        let Some(image) = build_leaf(&entries, prev, next, hk) else {
            return Err(Error::Runtime("index high key too large".into()));
        };
        write_image(bp, self.file, page, &image)
    }

    /// Rewrites an internal node's high key, preserving its first child and
    /// right sibling.
    fn set_internal_high_key(&self, bp: &BufferPool, page: PageNo, hk: &[u8]) -> Result<()> {
        let (first_child, next, entries) = bp.read_page(self.file, page, |p| {
            Ok((
                internal_first_child(p),
                internal_next(p),
                internal_entries(p).collect::<Vec<_>>(),
            ))
        })?;
        let Some(image) = build_internal(first_child, &entries, next, hk) else {
            return Err(Error::Runtime("index high key too large".into()));
        };
        write_image(bp, self.file, page, &image)
    }

    fn can_merge_leaves(&self, bp: &BufferPool, a: PageNo, b: PageNo) -> Result<bool> {
        let ua = bp.read_page(self.file, a, |p| Ok(leaf_bytes_used(p)))?;
        let ub = bp.read_page(self.file, b, |p| Ok(leaf_bytes_used(p)))?;
        Ok(ua + ub < PAGE_SIZE - 32)
    }

    fn can_merge_internals(
        &self,
        bp: &BufferPool,
        a: PageNo,
        b: PageNo,
        sep: &[u8],
    ) -> Result<bool> {
        let ua = bp.read_page(self.file, a, |p| Ok(internal_bytes_used(p)))?;
        let ub = bp.read_page(self.file, b, |p| Ok(internal_bytes_used(p)))?;
        // merging b into a appends `sep` and b's entries (minus b's header)
        let merged = ua + internal_entry_size(sep.len()) + ub.saturating_sub(internal_header_len(0));
        Ok(merged < PAGE_SIZE - 32)
    }

    pub fn search(&self, bp: &BufferPool, key: &[u8]) -> Result<Vec<Rid>> {
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

    fn descend(&self, bp: &BufferPool, mut page_no: PageNo, key: &[u8]) -> Result<PageNo> {
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

    /// Walks the whole tree checking the B-link invariants: leaves are sorted,
    /// each internal separator equals its child's minimum, every node's keys are
    /// `< high_key`, the rightmost sibling has no high key, and the leaf chain
    /// matches the tree. Used by the randomized model test.
    pub fn check_invariants(&self, bp: &BufferPool) -> Result<()> {
        let root = self.root(bp)?;
        if root == 0 {
            return Ok(());
        }
        let mut leaves = Vec::new();
        self.check_subtree(bp, root, &mut leaves)?;

        // the leaf chain must visit exactly the leaves the tree reaches, in order
        let first = self.header_u32(bp, FIRST_LEAF_OFF)?;
        let mut chain = Vec::new();
        let mut cur = first;
        let mut prev = 0;
        while cur != 0 {
            let (p, n) = bp.read_page(self.file, cur, |pg| Ok((leaf_prev(pg), leaf_next(pg))))?;
            if p != prev {
                return Err(Error::Runtime(format!("leaf {cur} prev {p} != {prev}")));
            }
            chain.push(cur);
            prev = cur;
            cur = n;
        }
        if chain != leaves {
            return Err(Error::Runtime("leaf chain does not match the tree".into()));
        }
        Ok(())
    }

    fn check_subtree(
        &self,
        bp: &BufferPool,
        page: PageNo,
        leaves: &mut Vec<PageNo>,
    ) -> Result<KeyRange> {
        let ty = bp.read_page(self.file, page, |p| Ok(node_type(p)))?;
        if ty == LEAF {
            leaves.push(page);
            let (n, hk, next) =
                bp.read_page(self.file, page, |p| Ok((leaf_num(p), leaf_high_key(p), leaf_next(p))))?;
            if n == 0 {
                if !hk.is_empty() {
                    return Err(Error::Runtime(format!("empty leaf {page} has a high key")));
                }
                return Ok((None, None));
            }
            let mut max: Option<Vec<u8>> = None;
            for i in 0..n {
                let (k, _) = bp.read_page(self.file, page, |p| Ok(leaf_entry_at(p, i)))?;
                if let Some(prev) = &max
                    && prev > &k
                {
                    return Err(Error::Runtime(format!("leaf {page} not sorted")));
                }
                max = Some(k);
            }
            let (min, max) = bp.read_page(self.file, page, |p| {
                Ok((leaf_entry_at(p, 0).0, leaf_entry_at(p, n - 1).0))
            })?;
            self.check_bound(bp, page, &hk, next, &max)?;
            Ok((Some(min), Some(max)))
        } else if ty == INTERNAL {
            let (n, hk, next, first) = bp.read_page(self.file, page, |p| {
                Ok((internal_num(p), internal_high_key(p), internal_next(p), internal_first_child(p)))
            })?;
            let entries: Vec<(Vec<u8>, PageNo)> =
                bp.read_page(self.file, page, |p| Ok(internal_entries(p).collect()))?;
            debug_assert_eq!(entries.len(), n);
            let mut children = vec![first];
            children.extend(entries.iter().map(|(_, c)| *c));

            let mut min = None;
            let mut max = None;
            for (i, child) in children.iter().enumerate() {
                let (cmin, cmax) = self.check_subtree(bp, *child, leaves)?;
                if i == 0 {
                    min = cmin.clone();
                }
                if i + 1 == children.len() {
                    max = cmax.clone();
                }
                if i > 0 {
                    let sep = &entries[i - 1].0;
                    match &cmin {
                        Some(m) if m != sep => {
                            return Err(Error::Runtime(format!(
                                "node {page} separator != child min"
                            )));
                        }
                        None => {
                            return Err(Error::Runtime(format!(
                                "node {page} separator over an empty subtree"
                            )));
                        }
                        _ => {}
                    }
                }
            }
            if let Some(mx) = &max {
                self.check_bound(bp, page, &hk, next, mx)?;
            }
            Ok((min, max))
        } else {
            Err(Error::Runtime(format!("corrupt index page {page}")))
        }
    }

    fn check_bound(
        &self,
        bp: &BufferPool,
        page: PageNo,
        hk: &[u8],
        next: PageNo,
        max: &[u8],
    ) -> Result<()> {
        if hk.is_empty() {
            if next != 0 {
                return Err(Error::Runtime(format!("node {page} unbounded but has a sibling")));
            }
            return Ok(());
        }
        if max >= hk {
            return Err(Error::Runtime(format!("node {page} has a key >= its high key")));
        }
        if next != 0 {
            match self.subtree_min(bp, next)? {
                Some(m) if m == hk => {}
                _ => {
                    return Err(Error::Runtime(format!(
                        "node {page} high key != right sibling min"
                    )));
                }
            }
        }
        Ok(())
    }

    fn subtree_min(&self, bp: &BufferPool, start: PageNo) -> Result<Option<Vec<u8>>> {
        let mut page = start;
        loop {
            let ty = bp.read_page(self.file, page, |p| Ok(node_type(p)))?;
            match ty {
                LEAF => {
                    return bp.read_page(self.file, page, |p| {
                        Ok(if leaf_num(p) == 0 { None } else { Some(leaf_entry_at(p, 0).0) })
                    });
                }
                INTERNAL => {
                    page =
                        bp.read_page(self.file, page, |p| Ok(internal_first_child(p)))?;
                }
                _ => return Err(Error::Runtime(format!("corrupt index page {page}"))),
            }
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

/// A lazy, forward-only reader over a B+ tree's leaf level. Yields rids in
/// ascending key order and holds only a `(leaf, position)` so it stops early.
pub struct LeafCursor {
    file: FileId,
    leaf: PageNo,
    pos: usize,
}

impl LeafCursor {
    /// The next rid in key order, or `None` at the end of the tree.
    pub fn next_rid(&mut self, bp: &BufferPool) -> Result<Option<Rid>> {
        loop {
            if self.leaf == 0 {
                return Ok(None);
            }
            let (file, leaf, pos) = (self.file, self.leaf, self.pos);
            let entry = bp.read_page(file, leaf, |page| {
                if pos < leaf_num(page) {
                    Ok(Some((leaf_entry_at(page, pos).1, leaf_next(page))))
                } else {
                    Ok(None)
                }
            })?;
            match entry {
                Some((rid, _)) => {
                    self.pos += 1;
                    return Ok(Some(rid));
                }
                None => {
                    self.leaf = bp.read_page(file, leaf, |page| Ok(leaf_next(page)))?;
                    self.pos = 0;
                }
            }
        }
    }
}

fn internal_sep_remove(
    bp: &BufferPool,
    file: FileId,
    page: PageNo,
    idx: usize,
) -> Result<()> {
    bp.with_page(file, page, |p| internal_remove_at(p, idx))
}
