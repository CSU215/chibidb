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

const MAGIC: [u8; 8] = *b"CHIDBIV1";
/// Root page number in the file header.
const ROOT_OFF: usize = header::HEADER_LEN;
/// First leaf page number in the file header.
const FIRST_LEAF_OFF: usize = header::HEADER_LEN + 4;
/// Longest encoded key inserted so far (u16), used to reserve separator space
/// when pre-splitting internal nodes top-down.
const MAX_KEY_OFF: usize = header::HEADER_LEN + 8;

/// A composite index key: the encoded value and the row id that makes it unique.
type Key = (Vec<u8>, Rid);
/// One internal separator: the child's minimum composite key and its page.
type Sep = (Vec<u8>, Rid, PageNo);

/// (smallest, largest) composite key of a subtree; both `None` when empty.
type KeyRange = (Option<Key>, Option<Key>);

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
fn build_leaf(entries: &[Key], prev: PageNo, next: PageNo, high_key: Option<&Key>) -> Option<PageData> {
    let mut page = zeroed_page();
    leaf_init(&mut page, prev, next);
    if let Some((hk, hk_rid)) = high_key
        && leaf_set_high_key(&mut page, hk, *hk_rid).is_err()
    {
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
    entries: &[Sep],
    next: PageNo,
    high_key: Option<&Key>,
) -> Option<PageData> {
    let mut page = zeroed_page();
    internal_init(&mut page, first_child);
    internal_set_next(&mut page, next);
    if let Some((hk, hk_rid)) = high_key
        && internal_set_high_key(&mut page, hk, *hk_rid).is_err()
    {
        return None;
    }
    for (i, (key, rid, child)) in entries.iter().enumerate() {
        if internal_insert_entry(&mut page, i, key, *rid, *child).is_err() {
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

fn hk_len(hk: Option<&Key>) -> usize {
    hk.map_or(0, |(k, _)| k.len())
}

/// Picks a leaf split point `m`: left `[..m]` carries high key `entries[m]`,
/// right `[m..]` keeps the old high key. Both halves must fit a page.
fn choose_leaf_split(all: &[Key], old_hk: Option<&Key>) -> Option<usize> {
    let sizes: Vec<usize> = all.iter().map(|(k, _)| leaf_entry_size(k.len())).collect();
    choose_split_with(&sizes, |m| leaf_header_len(all[m].0.len()), |_| {
        leaf_header_len(hk_len(old_hk))
    })
}

/// Picks an internal split point `m` (promoted separator `keys[m]`): left keeps
/// `keys[m]` as its high key, right keeps the old one.
fn choose_internal_split(keys: &[Key], old_hk: Option<&Key>) -> Option<usize> {
    let sizes: Vec<usize> = keys.iter().map(|(k, _)| internal_entry_size(k.len())).collect();
    let n = sizes.len();
    let mut prefix = vec![0usize; n + 1];
    for i in 0..n {
        prefix[i + 1] = prefix[i] + sizes[i];
    }
    let total = prefix[n];
    let mut best: Option<(usize, usize)> = None;
    for (m, window) in prefix.windows(2).enumerate().skip(1) {
        let left = internal_header_len(keys[m].0.len()) + window[0];
        let right = internal_header_len(hk_len(old_hk)) + (total - window[1]);
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
            u16_put(page, MAX_KEY_OFF, 0);
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
            u16_put(page, MAX_KEY_OFF, 0);
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

    /// Longest encoded key inserted so far, for the internal pre-split reserve.
    fn max_key_len(&self, bp: &BufferPool) -> Result<usize> {
        bp.read_page(self.file, 0, |page| Ok(u16_get(page, MAX_KEY_OFF)))
    }

    fn note_key_len(&self, bp: &BufferPool, len: usize) -> Result<()> {
        bp.with_page(self.file, 0, |page| {
            if len > u16_get(page, MAX_KEY_OFF) {
                u16_put(page, MAX_KEY_OFF, len);
            }
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
        self.note_key_len(bp, key.len())?;
        loop {
            // Read the reserve before taking any page latch: page 0 must never be
            // acquired while a node latch is held (it is the root-split lock).
            let maxk = self.max_key_len(bp)?;
            let full = |p: &[u8]| {
                if node_type(p) == LEAF {
                    leaf_bytes_used(p) + leaf_entry_size(key.len()) > PAGE_SIZE
                } else {
                    internal_bytes_used(p) + internal_entry_size(maxk) > PAGE_SIZE
                }
            };
            let root = self.root(bp)?;
            if root == 0 {
                // Create the first leaf; page 0's latch serializes the header.
                bp.with_page(self.file, 0, |hdr| {
                    if u32_get(hdr, ROOT_OFF) != 0 {
                        return Ok(());
                    }
                    let no = bp.alloc_page(self.file)?;
                    bp.with_page(self.file, no, |page| {
                        leaf_init(page, 0, 0);
                        Ok(())
                    })?;
                    u32_put(hdr, ROOT_OFF, no);
                    u32_put(hdr, FIRST_LEAF_OFF, no);
                    Ok(())
                })?;
                continue;
            }
            if !bp.read_page(self.file, root, |p| Ok(full(p)))? {
                if !self.insert_descend(bp, root, key, rid, maxk)? {
                    return Ok(());
                }
                // The entry node was split under us: re-read the root and retry,
                // which goes through the pre-split descent.
                continue;
            }
            // Split the root under page 0's latch so two threads cannot both
            // grow the tree.
            bp.with_page(self.file, 0, |hdr| {
                let cur = u32_get(hdr, ROOT_OFF);
                if bp.read_page(self.file, cur, |p| Ok(full(p)))? {
                    let new_root = self.split_root(bp, cur)?;
                    u32_put(hdr, ROOT_OFF, new_root);
                }
                Ok(())
            })?;
        }
    }

    /// Splits the root into two halves under a new root; returns the new root.
    fn split_root(&self, bp: &BufferPool, root: PageNo) -> Result<PageNo> {
        let new_no = bp.alloc_page(self.file)?;
        let (sep_key, sep_rid) = bp.with_page(self.file, root, |page| {
            if node_type(page) == LEAF {
                let all: Vec<Key> = leaf_entries(page).collect();
                let old_hk = leaf_high_key(page);
                let Some(m) = choose_leaf_split(&all, old_hk.as_ref()) else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                let sep = all[m].clone();
                let old_next = leaf_next(page);
                let Some(left) = build_leaf(&all[..m], 0, new_no, Some(&sep)) else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                let Some(right) = build_leaf(&all[m..], root, old_next, old_hk.as_ref()) else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                page.copy_from_slice(&left[..]);
                write_image(bp, self.file, new_no, &right)?;
                Ok((sep.0, sep.1))
            } else {
                let first = internal_first_child(page);
                let keys: Vec<Key> = internal_entries(page).map(|(k, r, _)| (k, r)).collect();
                let mut children = vec![first];
                children.extend(internal_entries(page).map(|(_, _, c)| c));
                let old_hk = internal_high_key(page);
                let old_next = internal_next(page);
                let Some(m) = choose_internal_split(&keys, old_hk.as_ref()) else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                let promoted = keys[m].clone();
                let left_entries: Vec<Sep> =
                    (0..m).map(|i| (keys[i].0.clone(), keys[i].1, children[i + 1])).collect();
                let right_entries: Vec<Sep> = ((m + 1)..keys.len())
                    .map(|i| (keys[i].0.clone(), keys[i].1, children[i + 1]))
                    .collect();
                let Some(left) = build_internal(children[0], &left_entries, new_no, Some(&promoted))
                else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                let Some(right) =
                    build_internal(children[m + 1], &right_entries, old_next, old_hk.as_ref())
                else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                page.copy_from_slice(&left[..]);
                write_image(bp, self.file, new_no, &right)?;
                Ok((promoted.0, promoted.1))
            }
        })?;
        let new_root = bp.alloc_page(self.file)?;
        bp.with_page(self.file, new_root, |page| {
            internal_init(page, root);
            internal_insert_entry(page, 0, &sep_key, sep_rid, new_no)
        })?;
        Ok(new_root)
    }

    /// Top-down insert: descends with latch coupling (the parent latch is held
    /// while the child is latched) and pre-splits any child that could not take
    /// another entry, so no split ever propagates upward.
    /// Returns `true` when the entry node was split by someone else and the
    /// insert must restart from the root.
    fn insert_descend(
        &self,
        bp: &BufferPool,
        page_no: PageNo,
        key: &[u8],
        rid: Rid,
        maxk: usize,
    ) -> Result<bool> {
        bp.with_page(self.file, page_no, |page| {
            let ty = node_type(page);
            // B-link: a concurrent split may have moved the key into a right
            // sibling that did not exist when we read our parent.
            let (hk, next) = if ty == LEAF {
                (leaf_high_key(page), leaf_next(page))
            } else {
                (internal_high_key(page), internal_next(page))
            };
            if let Some((hk_key, hk_rid)) = hk
                && crate::index::node::cmp_key(key, rid, &hk_key, hk_rid)
                    != std::cmp::Ordering::Less
            {
                if next == 0 {
                    return Err(Error::Runtime(
                        "corrupt index: bounded node without a right link".into(),
                    ));
                }
                return Ok(true);
            }
            // Self-check under the latch: between the parent's pre-split check
            // and our acquiring the latch, a concurrent insert may have filled
            // this node. Restart so the root pre-split / parent pre-split runs.
            let self_full = if ty == LEAF {
                leaf_bytes_used(page) + leaf_entry_size(key.len()) > PAGE_SIZE
            } else {
                internal_bytes_used(page) + internal_entry_size(maxk) > PAGE_SIZE
            };
            if self_full {
                return Ok(true);
            }
            if ty == LEAF {
                let idx = leaf_lower_bound(page, key, rid);
                leaf_insert_at(page, idx, key, rid)?;
                return Ok(false);
            }
            if ty != INTERNAL {
                return Err(Error::Runtime(format!("corrupt index page {page_no}")));
            }
            let mut child = internal_child_for(page, key, rid);
            let overflow = bp.read_page(self.file, child, |c| {
                Ok(if node_type(c) == LEAF {
                    leaf_bytes_used(c) + leaf_entry_size(key.len()) > PAGE_SIZE
                } else {
                    internal_bytes_used(c) + internal_entry_size(maxk) > PAGE_SIZE
                })
            })?;
            if overflow {
                let (descend, placed) = self.split_child(bp, page, child, key, rid)?;
                if placed {
                    return Ok(false);
                }
                child = descend;
            }
            // Recurse while still holding the parent latch (crabbing).
            self.insert_descend(bp, child, key, rid, maxk)
        })
    }

    /// Splits `child` and inserts the promoted separator into `parent` (held and
    /// roomy). Returns the half to descend into and whether the new entry is
    /// already placed (leaf split) or still has to be inserted (internal split).
    fn split_child(
        &self,
        bp: &BufferPool,
        parent: &mut [u8; PAGE_SIZE],
        child: PageNo,
        key: &[u8],
        rid: Rid,
    ) -> Result<(PageNo, bool)> {
        let new_no = bp.alloc_page(self.file)?;
        let (sep_key, sep_rid, descend, placed) = bp.with_page(self.file, child, |c| {
            if node_type(c) == LEAF {
                let mut all: Vec<Key> = leaf_entries(c).collect();
                let pos = all.partition_point(|(k, r)| {
                    crate::index::node::cmp_key(k, *r, key, rid) == std::cmp::Ordering::Less
                });
                all.insert(pos, (key.to_vec(), rid));
                let old_hk = leaf_high_key(c);
                let Some(m) = choose_leaf_split(&all, old_hk.as_ref()) else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                let sep = all[m].clone();
                let old_next = leaf_next(c);
                let Some(left) = build_leaf(&all[..m], leaf_prev(c), new_no, Some(&sep)) else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                let Some(right) = build_leaf(&all[m..], child, old_next, old_hk.as_ref()) else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                c.copy_from_slice(&left[..]);
                write_image(bp, self.file, new_no, &right)?;
                let descend = if crate::index::node::cmp_key(key, rid, &sep.0, sep.1)
                    == std::cmp::Ordering::Less
                {
                    child
                } else {
                    new_no
                };
                Ok((sep.0, sep.1, descend, true))
            } else {
                let first = internal_first_child(c);
                let keys: Vec<Key> = internal_entries(c).map(|(k, r, _)| (k, r)).collect();
                let mut children = vec![first];
                children.extend(internal_entries(c).map(|(_, _, ch)| ch));
                let old_hk = internal_high_key(c);
                let old_next = internal_next(c);
                let Some(m) = choose_internal_split(&keys, old_hk.as_ref()) else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                let promoted = keys[m].clone();
                let left_entries: Vec<Sep> =
                    (0..m).map(|i| (keys[i].0.clone(), keys[i].1, children[i + 1])).collect();
                let right_entries: Vec<Sep> = ((m + 1)..keys.len())
                    .map(|i| (keys[i].0.clone(), keys[i].1, children[i + 1]))
                    .collect();
                let Some(left) = build_internal(children[0], &left_entries, new_no, Some(&promoted))
                else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                let Some(right) =
                    build_internal(children[m + 1], &right_entries, old_next, old_hk.as_ref())
                else {
                    return Err(Error::Runtime("index entry too large to split".into()));
                };
                c.copy_from_slice(&left[..]);
                write_image(bp, self.file, new_no, &right)?;
                let descend = if crate::index::node::cmp_key(key, rid, &promoted.0, promoted.1)
                    == std::cmp::Ordering::Less
                {
                    child
                } else {
                    new_no
                };
                Ok((promoted.0, promoted.1, descend, false))
            }
        })?;
        let idx = if child == internal_first_child(parent) {
            0
        } else {
            internal_entries(parent)
                .position(|(_, _, c)| c == child)
                .map(|i| i + 1)
                .ok_or_else(|| Error::Runtime("corrupt index: child not found".into()))?
        };
        internal_insert_entry(parent, idx, &sep_key, sep_rid, new_no)?;
        Ok((descend, placed))
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
            Bound::Included(key) => {
                // (key, 0) is the first composite with this key
                let leaf = self.descend(bp, root, key, Rid::new(0, 0))?;
                let pos =
                    bp.read_page(self.file, leaf, |page| Ok(leaf_lower_bound(page, key, Rid::new(0, 0))))?;
                (leaf, pos)
            }
            Bound::Excluded(key) => {
                // start after every rid for this key
                let hi = Rid::new(u32::MAX, u16::MAX);
                let leaf = self.descend(bp, root, key, hi)?;
                let pos = bp.read_page(self.file, leaf, |page| Ok(leaf_upper_bound(page, key, hi)))?;
                (leaf, pos)
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
                            self.set_leaf_high_key(bp, first_child, None)?;
                            self.set_leaf_next(bp, first_child, None)?;
                        } else {
                            self.set_internal_high_key(bp, first_child, None)?;
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
                let i = leaf_lower_bound(page, key, rid);
                if i < leaf_num(page) {
                    let (k, r) = leaf_entry_at(page, i);
                    if k.as_slice() == key && r == rid {
                        leaf_remove_at(page, i)?;
                        return Ok(DeleteOutcome::Deleted {
                            underflow: leaf_bytes_used(page) < MIN_OCCUPANCY,
                        });
                    }
                }
                Ok(DeleteOutcome::NotFound)
            })
        } else if ty == INTERNAL {
            // Composite keys are unique, so exactly one child can hold the pair.
            let (child, child_idx) = bp.read_page(self.file, page_no, |page| {
                let child = internal_child_for(page, key, rid);
                let mut children = vec![internal_first_child(page)];
                children.extend(internal_entries(page).map(|(_, _, c)| c));
                let idx = children.iter().position(|&c| c == child).unwrap_or(0);
                Ok((child, idx))
            })?;
            match self.delete_rec(bp, child, key, rid)? {
                DeleteOutcome::NotFound => Ok(DeleteOutcome::NotFound),
                DeleteOutcome::Deleted { underflow } => {
                    if underflow {
                        self.fix_child(bp, page_no, child)?;
                    } else {
                        // the child's minimum may have changed: keep the
                        // separator on its left equal to it
                        self.refresh_separator(bp, page_no, child_idx)?;
                    }
                    let parent_underflow = bp.read_page(self.file, page_no, |page| {
                        Ok(internal_bytes_used(page) < MIN_OCCUPANCY)
                    })?;
                    Ok(DeleteOutcome::Deleted { underflow: parent_underflow })
                }
            }
        } else {
            Err(Error::Runtime(format!("corrupt index page {page_no}")))
        }
    }

    /// Ensures the separator left of child `child_idx` equals that child's
    /// minimum composite. The child to the left of that separator carries it as
    /// its own high key, so both are updated together.
    fn refresh_separator(&self, bp: &BufferPool, parent: PageNo, child_idx: usize) -> Result<()> {
        if child_idx == 0 {
            return Ok(());
        }
        let children = bp.read_page(self.file, parent, |page| {
            let mut children = vec![internal_first_child(page)];
            children.extend(internal_entries(page).map(|(_, _, c)| c));
            Ok(children)
        })?;
        let (Some(&child), Some(&left)) = (children.get(child_idx), children.get(child_idx - 1))
        else {
            return Ok(());
        };
        if let Some(min) = self.subtree_first_key(bp, child)? {
            self.set_separator_key(bp, parent, child_idx - 1, min.clone())?;
            let left_ty = bp.read_page(self.file, left, |p| Ok(node_type(p)))?;
            if left_ty == LEAF {
                self.set_leaf_high_key(bp, left, Some(&min))?;
            } else {
                self.set_internal_high_key(bp, left, Some(&min))?;
            }
        }
        Ok(())
    }

    /// The smallest composite key in a page's subtree (its leftmost leaf's
    /// first entry).
    fn subtree_first_key(&self, bp: &BufferPool, start: PageNo) -> Result<Option<Key>> {
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
                        Some(leaf_entry_at(p, 0))
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
            children.extend(internal_entries(page).map(|(_, _, c)| c));
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
                    let k = self.last_leaf_entry(bp, left)?;
                    // only borrow if the entry actually fits the receiver
                    if child_used + leaf_entry_size(k.0.len()) <= PAGE_SIZE {
                        let k = self.pop_last_leaf_entry(bp, left)?;
                        self.prepend_leaf_entry(bp, child, &k.0, k.1)?;
                        // the moved key is now the child's minimum, i.e. the
                        // left sibling's high key
                        self.set_leaf_high_key(bp, left, Some(&k))?;
                        self.set_separator_key(bp, parent, idx - 1, k)?;
                        continue;
                    }
                }
            }
            if let Some(right) = right {
                let used = bp.read_page(self.file, right, |page| Ok(leaf_bytes_used(page)))?;
                if used > MIN_OCCUPANCY + 64 {
                    let k = self.first_leaf_entry(bp, right)?;
                    if child_used + leaf_entry_size(k.0.len()) <= PAGE_SIZE {
                        let k = self.take_first_leaf_entry(bp, right)?;
                        self.append_leaf_entry(bp, child, &k.0, k.1)?;
                        let first = self.first_leaf_entry(bp, right)?;
                        // the child now covers up to the right sibling's new min
                        self.set_leaf_high_key(bp, child, Some(&first))?;
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
            self.set_leaf_high_key(bp, left, child_hk.as_ref())?;
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
                self.set_leaf_high_key(bp, child, right_hk.as_ref())?;
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
                    if child_used + internal_entry_size(psep.0.len()) <= PAGE_SIZE {
                        let (lk, lc) = self.pop_last_separator(bp, left)?;
                        let old_first =
                            bp.read_page(self.file, child, |page| Ok(internal_first_child(page)))?;
                        bp.with_page(self.file, child, |page| {
                            internal_insert_entry(page, 0, &psep.0, psep.1, old_first)?;
                            internal_set_first_child(page, lc);
                            Ok(())
                        })?;
                        // the moved separator is now the left sibling's high key
                        self.set_internal_high_key(bp, left, Some(&lk))?;
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
                    if child_used + internal_entry_size(psep.0.len()) <= PAGE_SIZE {
                        let (rk, rc) = self.take_first_separator(bp, right)?;
                        let rfirst =
                            bp.read_page(self.file, right, |page| Ok(internal_first_child(page)))?;
                        bp.with_page(self.file, child, |page| {
                            let n = internal_num(page);
                            internal_insert_entry(page, n, &psep.0, psep.1, rfirst)
                        })?;
                        self.set_internal_first_child(bp, right, rc)?;
                        // the child now covers up to the right sibling's new min
                        self.set_internal_high_key(bp, child, Some(&rk))?;
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
                entries.push((psep.0, psep.1, child_first));
                entries.extend(child_entries);
                let Some(image) = build_internal(lfirst, &entries, child_next, child_hk.as_ref())
                else {
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
                entries.push((psep.0, psep.1, rfirst));
                entries.extend(rentries);
                let Some(image) = build_internal(cfirst, &entries, right_next, right_hk.as_ref())
                else {
                    return Err(Error::Runtime("index merge overflow".into()));
                };
                write_image(bp, self.file, child, &image)?;
                return internal_sep_remove(bp, self.file, parent, idx);
            }
            return Err(Error::Runtime("index merge overflow".into()));
        }
        Ok(())
    }

    fn read_internal(&self, bp: &BufferPool, page: PageNo) -> Result<(PageNo, Vec<Sep>)> {
        bp.read_page(self.file, page, |p| {
            Ok((internal_first_child(p), internal_entries(p).collect()))
        })
    }

    fn pop_last_separator(&self, bp: &BufferPool, page: PageNo) -> Result<(Key, PageNo)> {
        let (n, entry) = bp.read_page(self.file, page, |p| {
            Ok((internal_num(p), internal_entry_at(p, internal_num(p) - 1)))
        })?;
        bp.with_page(self.file, page, |p| internal_remove_at(p, n - 1))?;
        Ok(((entry.0, entry.1), entry.2))
    }

    fn take_first_separator(&self, bp: &BufferPool, page: PageNo) -> Result<(Key, PageNo)> {
        let entry = bp.read_page(self.file, page, |p| Ok(internal_entry_at(p, 0)))?;
        bp.with_page(self.file, page, |p| {
            internal_remove_at(p, 0)?;
            Ok(())
        })?;
        Ok(((entry.0, entry.1), entry.2))
    }

    fn set_internal_first_child(&self, bp: &BufferPool, page: PageNo, child: PageNo) -> Result<()> {
        bp.with_page(self.file, page, |p| {
            internal_set_first_child(p, child);
            Ok(())
        })
    }

    /// The composite key of separator `sep_idx` in `parent`.
    fn separator_key(&self, bp: &BufferPool, parent: PageNo, sep_idx: usize) -> Result<Key> {
        bp.read_page(self.file, parent, |p| {
            let (k, r, _) = internal_entry_at(p, sep_idx);
            Ok((k, r))
        })
    }

    /// Replaces one separator composite by rebuilding the page in scratch and
    /// swapping it in, so a failed build never mutates the live page.
    fn set_separator_key(
        &self,
        bp: &BufferPool,
        parent: PageNo,
        sep_idx: usize,
        new_key: Key,
    ) -> Result<()> {
        let (first_child, mut entries) = self.read_internal(bp, parent)?;
        if sep_idx >= entries.len() {
            return Err(Error::Runtime("corrupt index: separator out of range".into()));
        }
        let (next, hk) =
            bp.read_page(self.file, parent, |p| Ok((internal_next(p), internal_high_key(p))))?;
        entries[sep_idx].0 = new_key.0;
        entries[sep_idx].1 = new_key.1;
        let Some(image) = build_internal(first_child, &entries, next, hk.as_ref()) else {
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
    fn set_leaf_high_key(&self, bp: &BufferPool, page: PageNo, hk: Option<&Key>) -> Result<()> {
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
    fn set_internal_high_key(&self, bp: &BufferPool, page: PageNo, hk: Option<&Key>) -> Result<()> {
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

    fn can_merge_internals(&self, bp: &BufferPool, a: PageNo, b: PageNo, sep: &Key) -> Result<bool> {
        let ua = bp.read_page(self.file, a, |p| Ok(internal_bytes_used(p)))?;
        let (ub, hk_b) =
            bp.read_page(self.file, b, |p| Ok((internal_bytes_used(p), internal_high_key(p))))?;
        // merging b into a appends `sep` and b's entries (minus b's header)
        let b_entries = ub.saturating_sub(internal_header_len(hk_len(hk_b.as_ref())));
        let merged = ua + internal_entry_size(sep.0.len()) + b_entries;
        Ok(merged < PAGE_SIZE - 32)
    }

    pub fn search(&self, bp: &BufferPool, key: &[u8]) -> Result<Vec<Rid>> {
        let root = self.root(bp)?;
        if root == 0 {
            return Ok(vec![]);
        }
        // (key, 0) is the first composite with this key; no backward walk needed
        let mut page_no = self.descend(bp, root, key, Rid::new(0, 0))?;
        let mut out = Vec::new();
        loop {
            let next = bp.read_page(self.file, page_no, |page| {
                let mut i = leaf_lower_bound(page, key, Rid::new(0, 0));
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

    /// Descends to the leaf that should hold `key`. B-link "lock-fetch": if
    /// `key` is at or past a node's high key the key belongs to that node's
    /// right sibling, so follow `next` and retry. This keeps a descent correct
    /// even if a split moved the key right after we read its parent.
    fn descend(&self, bp: &BufferPool, mut page_no: PageNo, key: &[u8], rid: Rid) -> Result<PageNo> {
        loop {
            let (ty, hk, next) = bp.read_page(self.file, page_no, |page| {
                let ty = node_type(page);
                let hk = if ty == LEAF {
                    leaf_high_key(page)
                } else {
                    internal_high_key(page)
                };
                let next = if ty == LEAF { leaf_next(page) } else { internal_next(page) };
                Ok((ty, hk, next))
            })?;
            if let Some((hk_key, hk_rid)) = hk
                && crate::index::node::cmp_key(key, rid, &hk_key, hk_rid) != std::cmp::Ordering::Less
            {
                if next == 0 {
                    return Err(Error::Runtime(format!(
                        "corrupt index: page {page_no} bounded without a right link"
                    )));
                }
                page_no = next;
                continue;
            }
            match ty {
                LEAF => return Ok(page_no),
                INTERNAL => {
                    page_no = bp
                        .read_page(self.file, page_no, |page| Ok(internal_child_for(page, key, rid)))?;
                }
                _ => return Err(Error::Runtime(format!("corrupt index page {page_no}"))),
            }
        }
    }

    /// Walks the whole tree checking the B-link invariants: leaves are sorted,
    /// each internal separator equals its child's minimum, every node's keys are
    /// `< high_key`, the rightmost sibling has no high key, and the B-link leaf
    /// chain matches the tree. Used by the randomized model test.
    pub fn check_invariants(&self, bp: &BufferPool) -> Result<()> {
        let root = self.root(bp)?;
        if root == 0 {
            return Ok(());
        }
        let mut leaves = Vec::new();
        self.check_subtree(bp, root, &mut leaves)?;

        // the right-link chain must visit exactly the leaves the tree reaches
        let first = self.header_u32(bp, FIRST_LEAF_OFF)?;
        let mut chain = Vec::new();
        let mut cur = first;
        while cur != 0 {
            chain.push(cur);
            cur = bp.read_page(self.file, cur, |pg| Ok(leaf_next(pg)))?;
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
                if hk.is_some() {
                    return Err(Error::Runtime(format!("empty leaf {page} has a high key")));
                }
                return Ok((None, None));
            }
            let mut max: Option<Key> = None;
            for i in 0..n {
                let e = bp.read_page(self.file, page, |p| Ok(leaf_entry_at(p, i)))?;
                if let Some(prev) = &max
                    && prev > &e
                {
                    return Err(Error::Runtime(format!("leaf {page} not sorted")));
                }
                max = Some(e);
            }
            let (min, max) = bp.read_page(self.file, page, |p| {
                Ok((leaf_entry_at(p, 0), leaf_entry_at(p, n - 1)))
            })?;
            self.check_bound(bp, page, hk.as_ref(), next, &max)?;
            Ok((Some(min), Some(max)))
        } else if ty == INTERNAL {
            let (n, hk, next, first) = bp.read_page(self.file, page, |p| {
                Ok((internal_num(p), internal_high_key(p), internal_next(p), internal_first_child(p)))
            })?;
            let entries: Vec<Sep> =
                bp.read_page(self.file, page, |p| Ok(internal_entries(p).collect()))?;
            debug_assert_eq!(entries.len(), n);
            let mut children = vec![first];
            children.extend(entries.iter().map(|(_, _, c)| *c));

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
                    let sep = (entries[i - 1].0.clone(), entries[i - 1].1);
                    match &cmin {
                        Some(m) if *m != sep => {
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
                self.check_bound(bp, page, hk.as_ref(), next, mx)?;
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
        hk: Option<&Key>,
        next: PageNo,
        max: &Key,
    ) -> Result<()> {
        let Some(hk) = hk else {
            if next != 0 {
                return Err(Error::Runtime(format!("node {page} unbounded but has a sibling")));
            }
            return Ok(());
        };
        if max >= hk {
            return Err(Error::Runtime(format!("node {page} has a key >= its high key")));
        }
        if next != 0 {
            match self.subtree_min(bp, next)? {
                Some(m) if &m == hk => {}
                _ => {
                    return Err(Error::Runtime(format!(
                        "node {page} high key != right sibling min"
                    )));
                }
            }
        }
        Ok(())
    }

    fn subtree_min(&self, bp: &BufferPool, start: PageNo) -> Result<Option<Key>> {
        let mut page = start;
        loop {
            let ty = bp.read_page(self.file, page, |p| Ok(node_type(p)))?;
            match ty {
                LEAF => {
                    return bp.read_page(self.file, page, |p| {
                        Ok(if leaf_num(p) == 0 { None } else { Some(leaf_entry_at(p, 0)) })
                    });
                }
                INTERNAL => {
                    page = bp.read_page(self.file, page, |p| Ok(internal_first_child(p)))?;
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

fn u16_get(page: &[u8], off: usize) -> usize {
    u16::from_le_bytes([page[off], page[off + 1]]) as usize
}

fn u16_put(page: &mut [u8], off: usize, v: usize) {
    page[off..off + 2].copy_from_slice(&(v as u16).to_le_bytes());
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
