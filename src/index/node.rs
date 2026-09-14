//! B-link index node layout.
//!
//! The tree orders entries by the composite key `(key, rid)`. `key` is the
//! order-preserving encoding of a column value; `rid` breaks ties so every
//! entry is unique, which lets a plain B-link descent (no previous-leaf walk)
//! handle duplicate index values. Internal separators and high keys therefore
//! store the rid too.
//!
//! Every node also carries a `next` page (its right sibling, the B-link) and a
//! boundary high key: any `(key, rid) >= high key` belongs to a node to the
//! right. A zero-length high key means "unbounded" (rightmost node / root).
//!
//! leaf header:     type@0 num@1..3 prev@3..7 next@7..11 hk@13.. entries@...
//! internal header: type@0 num@1..3 first_child@3..7 next@7..11 hk@13.. entries@...
//!   high key region: [key_len u16]@11..13 [key][page u32][slot u16]
//!   (empty when key_len == 0)

use std::cmp::Ordering;

use crate::storage::{PageNo, Rid, PAGE_SIZE};
use crate::{Error, Result};

pub const LEAF: u8 = 0;
pub const INTERNAL: u8 = 1;

const MAX_KEY_LEN: usize = u16::MAX as usize;

const LEAF_BASE: usize = 13;
const INTERNAL_BASE: usize = 13;
const ENTRY_OVERHEAD: usize = 2; // key_len u16
const RID_LEN: usize = 6; // page u32 + slot u16

/// Ordered comparison of two `(key, rid)` composites.
pub fn cmp_key(a_key: &[u8], a_rid: Rid, b_key: &[u8], b_rid: Rid) -> Ordering {
    a_key.cmp(b_key).then_with(|| a_rid.cmp(&b_rid))
}

/// Encoded size of one leaf entry with a `key_len`-byte key.
pub const fn leaf_entry_size(key_len: usize) -> usize {
    ENTRY_OVERHEAD + key_len + RID_LEN
}

/// Encoded size of one internal entry with a `key_len`-byte key.
pub const fn internal_entry_size(key_len: usize) -> usize {
    ENTRY_OVERHEAD + key_len + RID_LEN + 4
}

fn u16_at(page: &[u8], off: usize) -> usize {
    u16::from_le_bytes([page[off], page[off + 1]]) as usize
}

fn u16_set(page: &mut [u8], off: usize, v: usize) {
    page[off..off + 2].copy_from_slice(&(v as u16).to_le_bytes());
}

fn u32_at(page: &[u8], off: usize) -> PageNo {
    u32::from_le_bytes(page[off..off + 4].try_into().unwrap())
}

fn u32_set(page: &mut [u8], off: usize, v: PageNo) {
    page[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn node_type(page: &[u8]) -> u8 {
    page[0]
}

// ------------------------------------------------------------- high key

fn hk_key_len(page: &[u8]) -> usize {
    u16_at(page, 11)
}

/// (key, rid) of the high key, or `None` when unbounded.
fn high_key(page: &[u8]) -> Option<(Vec<u8>, Rid)> {
    let l = hk_key_len(page);
    if l == 0 {
        return None;
    }
    let key = page[LEAF_BASE..LEAF_BASE + l].to_vec();
    let base = LEAF_BASE + l;
    let rid = Rid::new(u32_at(page, base), u16_at(page, base + 4) as u16);
    Some((key, rid))
}

fn set_high_key(page: &mut [u8; PAGE_SIZE], key: &[u8], rid: Rid) -> Result<()> {
    if key.len() > MAX_KEY_LEN || LEAF_BASE + key.len() + RID_LEN > PAGE_SIZE {
        return Err(Error::Runtime("index high key too large".into()));
    }
    u16_set(page, 11, key.len());
    page[LEAF_BASE..LEAF_BASE + key.len()].copy_from_slice(key);
    let base = LEAF_BASE + key.len();
    u32_set(page, base, rid.page_no);
    u16_set(page, base + 4, rid.slot as usize);
    Ok(())
}

fn high_key_bytes(key_len: usize) -> usize {
    if key_len == 0 { 0 } else { key_len + RID_LEN }
}

/// Bytes occupied by a leaf header, for a given high-key length.
pub const fn leaf_header_len(high_key_len: usize) -> usize {
    if high_key_len == 0 {
        LEAF_BASE
    } else {
        LEAF_BASE + high_key_len + RID_LEN
    }
}

/// Bytes occupied by an internal header, for a given high-key length.
pub const fn internal_header_len(high_key_len: usize) -> usize {
    if high_key_len == 0 {
        INTERNAL_BASE
    } else {
        INTERNAL_BASE + high_key_len + RID_LEN
    }
}

// ---------------------------------------------------------------- leaf

pub fn leaf_init(page: &mut [u8; PAGE_SIZE], prev: PageNo, next: PageNo) {
    page[0] = LEAF;
    u16_set(page, 1, 0);
    u32_set(page, 3, prev);
    u32_set(page, 7, next);
    u16_set(page, 11, 0);
}

pub fn leaf_prev(page: &[u8]) -> PageNo {
    u32_at(page, 3)
}

pub fn leaf_next(page: &[u8]) -> PageNo {
    u32_at(page, 7)
}

pub fn leaf_set_next(page: &mut [u8], next: PageNo) {
    u32_set(page, 7, next);
}

pub fn leaf_set_prev(page: &mut [u8], prev: PageNo) {
    u32_set(page, 3, prev);
}

pub fn leaf_high_key(page: &[u8]) -> Option<(Vec<u8>, Rid)> {
    high_key(page)
}

/// Sets the leaf's high key. Only valid on an empty leaf (it shifts the entry
/// area); builders set it before inserting entries.
pub fn leaf_set_high_key(page: &mut [u8; PAGE_SIZE], key: &[u8], rid: Rid) -> Result<()> {
    set_high_key(page, key, rid)
}

pub fn leaf_num(page: &[u8]) -> usize {
    u16_at(page, 1)
}

fn leaf_entries_start(page: &[u8]) -> usize {
    LEAF_BASE + high_key_bytes(hk_key_len(page))
}

fn leaf_entry_offset(page: &[u8], i: usize) -> usize {
    let mut off = leaf_entries_start(page);
    for _ in 0..i {
        let key_len = u16_at(page, off);
        off += ENTRY_OVERHEAD + key_len + RID_LEN;
    }
    off
}

pub fn leaf_entry_at(page: &[u8], i: usize) -> (Vec<u8>, Rid) {
    let off = leaf_entry_offset(page, i);
    let key_len = u16_at(page, off);
    let key = page[off + 2..off + 2 + key_len].to_vec();
    let base = off + 2 + key_len;
    let page_no = u32_at(page, base);
    let slot = u16_at(page, base + 4) as u16;
    (key, Rid::new(page_no, slot))
}

/// First entry index with `(key, rid) >= (search_key, search_rid)`.
pub fn leaf_lower_bound(page: &[u8], key: &[u8], rid: Rid) -> usize {
    leaf_bound(page, key, rid, false)
}

/// First entry index with `(key, rid) > (search_key, search_rid)`.
pub fn leaf_upper_bound(page: &[u8], key: &[u8], rid: Rid) -> usize {
    leaf_bound(page, key, rid, true)
}

fn leaf_bound(page: &[u8], key: &[u8], rid: Rid, strict: bool) -> usize {
    let n = leaf_num(page);
    let mut off = leaf_entries_start(page);
    for i in 0..n {
        let key_len = u16_at(page, off);
        let k = &page[off + 2..off + 2 + key_len];
        let base = off + 2 + key_len;
        let r = Rid::new(u32_at(page, base), u16_at(page, base + 4) as u16);
        let c = cmp_key(k, r, key, rid);
        let past = if strict { c == Ordering::Greater } else { c != Ordering::Less };
        if past {
            return i;
        }
        off += ENTRY_OVERHEAD + key_len + RID_LEN;
    }
    n
}

pub fn leaf_entries<'a>(page: &'a [u8]) -> impl Iterator<Item = (Vec<u8>, Rid)> + 'a {
    let n = leaf_num(page) as u16;
    (0..n).map(move |i| leaf_entry_at(page, i as usize))
}

pub fn leaf_insert_at(page: &mut [u8; PAGE_SIZE], idx: usize, key: &[u8], rid: Rid) -> Result<()> {
    if key.len() > MAX_KEY_LEN {
        return Err(Error::Runtime("index key too large".into()));
    }
    let n = leaf_num(page);
    let end = leaf_entry_offset(page, n);
    let need = ENTRY_OVERHEAD + key.len() + RID_LEN;
    if end + need > PAGE_SIZE {
        return Err(Error::PageFull);
    }
    let at = leaf_entry_offset(page, idx);
    page.copy_within(at..end, at + need);
    u16_set(page, at, key.len());
    page[at + 2..at + 2 + key.len()].copy_from_slice(key);
    let base = at + 2 + key.len();
    u32_set(page, base, rid.page_no);
    u16_set(page, base + 4, rid.slot as usize);
    u16_set(page, 1, n + 1);
    Ok(())
}

pub fn leaf_remove_at(page: &mut [u8; PAGE_SIZE], idx: usize) -> Result<()> {
    let n = leaf_num(page);
    if idx >= n {
        return Err(Error::Runtime(format!("no entry at index {idx}")));
    }
    let at = leaf_entry_offset(page, idx);
    let key_len = u16_at(page, at);
    let size = ENTRY_OVERHEAD + key_len + RID_LEN;
    let end = leaf_entry_offset(page, n);
    page.copy_within(at + size..end, at);
    u16_set(page, 1, n - 1);
    Ok(())
}

pub fn leaf_bytes_used(page: &[u8]) -> usize {
    leaf_entry_offset(page, leaf_num(page))
}

// ------------------------------------------------------------ internal

pub fn internal_init(page: &mut [u8; PAGE_SIZE], first_child: PageNo) {
    page[0] = INTERNAL;
    u16_set(page, 1, 0);
    u32_set(page, 3, first_child);
    u32_set(page, 7, 0);
    u16_set(page, 11, 0);
}

pub fn internal_first_child(page: &[u8]) -> PageNo {
    u32_at(page, 3)
}

pub fn internal_set_first_child(page: &mut [u8], child: PageNo) {
    u32_set(page, 3, child);
}

/// The internal node's right sibling (B-link).
pub fn internal_next(page: &[u8]) -> PageNo {
    u32_at(page, 7)
}

pub fn internal_set_next(page: &mut [u8; PAGE_SIZE], next: PageNo) {
    u32_set(page, 7, next);
}

pub fn internal_high_key(page: &[u8]) -> Option<(Vec<u8>, Rid)> {
    high_key(page)
}

pub fn internal_set_high_key(page: &mut [u8; PAGE_SIZE], key: &[u8], rid: Rid) -> Result<()> {
    set_high_key(page, key, rid)
}

pub fn internal_num(page: &[u8]) -> usize {
    u16_at(page, 1)
}

fn internal_entries_start(page: &[u8]) -> usize {
    INTERNAL_BASE + high_key_bytes(hk_key_len(page))
}

fn internal_entry_offset(page: &[u8], i: usize) -> usize {
    let mut off = internal_entries_start(page);
    for _ in 0..i {
        let key_len = u16_at(page, off);
        off += ENTRY_OVERHEAD + key_len + RID_LEN + 4;
    }
    off
}

/// (separator composite key, separator rid, child page).
pub fn internal_entry_at(page: &[u8], i: usize) -> (Vec<u8>, Rid, PageNo) {
    let off = internal_entry_offset(page, i);
    let key_len = u16_at(page, off);
    let key = page[off + 2..off + 2 + key_len].to_vec();
    let base = off + 2 + key_len;
    let rid = Rid::new(u32_at(page, base), u16_at(page, base + 4) as u16);
    let child = u32_at(page, base + RID_LEN);
    (key, rid, child)
}

pub fn internal_entries<'a>(page: &'a [u8]) -> impl Iterator<Item = (Vec<u8>, Rid, PageNo)> + 'a {
    let n = internal_num(page) as u16;
    (0..n).map(move |i| internal_entry_at(page, i as usize))
}

/// Child page covering `(key, rid)`: separators route composites `>= sep`
/// rightwards.
pub fn internal_child_for(page: &[u8], key: &[u8], rid: Rid) -> PageNo {
    let n = internal_num(page);
    let mut child = internal_first_child(page);
    let mut off = internal_entries_start(page);
    for _ in 0..n {
        let key_len = u16_at(page, off);
        let sep = &page[off + 2..off + 2 + key_len];
        let base = off + 2 + key_len;
        let sep_rid = Rid::new(u32_at(page, base), u16_at(page, base + 4) as u16);
        if cmp_key(key, rid, sep, sep_rid) == Ordering::Less {
            break;
        }
        child = u32_at(page, base + RID_LEN);
        off += ENTRY_OVERHEAD + key_len + RID_LEN + 4;
    }
    child
}

pub fn internal_insert_entry(
    page: &mut [u8; PAGE_SIZE],
    idx: usize,
    key: &[u8],
    rid: Rid,
    child: PageNo,
) -> Result<()> {
    let n = internal_num(page);
    let end = internal_entry_offset(page, n);
    let need = ENTRY_OVERHEAD + key.len() + RID_LEN + 4;
    if end + need > PAGE_SIZE {
        return Err(Error::PageFull);
    }
    let at = internal_entry_offset(page, idx);
    page.copy_within(at..end, at + need);
    u16_set(page, at, key.len());
    page[at + 2..at + 2 + key.len()].copy_from_slice(key);
    let base = at + 2 + key.len();
    u32_set(page, base, rid.page_no);
    u16_set(page, base + 4, rid.slot as usize);
    u32_set(page, base + RID_LEN, child);
    u16_set(page, 1, n + 1);
    Ok(())
}

pub fn internal_remove_at(page: &mut [u8; PAGE_SIZE], idx: usize) -> Result<()> {
    let n = internal_num(page);
    if idx >= n {
        return Err(Error::Runtime(format!("no separator at index {idx}")));
    }
    let at = internal_entry_offset(page, idx);
    let key_len = u16_at(page, at);
    let size = ENTRY_OVERHEAD + key_len + RID_LEN + 4;
    let end = internal_entry_offset(page, n);
    page.copy_within(at + size..end, at);
    u16_set(page, 1, n - 1);
    Ok(())
}

pub fn internal_bytes_used(page: &[u8]) -> usize {
    internal_entry_offset(page, internal_num(page))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::page::zeroed_page;

    fn rid(page: u32, slot: u16) -> Rid {
        Rid::new(page, slot)
    }

    fn leaf_with(keys: &[&[u8]]) -> [u8; PAGE_SIZE] {
        let mut page: [u8; PAGE_SIZE] = *zeroed_page();
        leaf_init(&mut page, 0, 0);
        for (i, key) in keys.iter().enumerate() {
            leaf_insert_at(&mut page, i, key, rid(1, i as u16)).unwrap();
        }
        page
    }

    #[test]
    fn lower_and_upper_bound_cover_duplicates_and_edges() {
        let page = leaf_with(&[b"\x01", b"\x03", b"\x03", b"\x03", b"\x07", b"\x09"]);
        assert_eq!(leaf_lower_bound(&page, b"\x03", rid(0, 0)), 1);
        assert_eq!(leaf_lower_bound(&page, b"\x03", rid(1, 2)), 2);
        assert_eq!(leaf_lower_bound(&page, b"\x00", rid(0, 0)), 0);
        assert_eq!(leaf_lower_bound(&page, b"\x0a", rid(0, 0)), 6);
        assert_eq!(leaf_upper_bound(&page, b"\x03", rid(1, 3)), 4);
    }

    #[test]
    fn internal_child_for_picks_the_covering_child() {
        let mut page: [u8; PAGE_SIZE] = *zeroed_page();
        internal_init(&mut page, 10);
        internal_insert_entry(&mut page, 0, b"\x03", rid(0, 0), 11).unwrap();
        internal_insert_entry(&mut page, 1, b"\x05", rid(0, 0), 12).unwrap();
        assert_eq!(internal_child_for(&page, b"\x00", rid(0, 0)), 10);
        assert_eq!(internal_child_for(&page, b"\x03", rid(0, 0)), 11);
        assert_eq!(internal_child_for(&page, b"\x04", rid(0, 0)), 11);
        assert_eq!(internal_child_for(&page, b"\x05", rid(0, 0)), 12);
        assert_eq!(internal_child_for(&page, b"\xff", rid(0, 0)), 12);
    }

    #[test]
    fn a_high_key_shifts_the_entry_area_without_corrupting_entries() {
        let mut page: [u8; PAGE_SIZE] = *zeroed_page();
        leaf_init(&mut page, 0, 9);
        leaf_set_high_key(&mut page, b"m", rid(0, 7)).unwrap();
        leaf_insert_at(&mut page, 0, b"a", rid(1, 1)).unwrap();
        leaf_insert_at(&mut page, 1, b"z", rid(2, 2)).unwrap();
        assert_eq!(leaf_high_key(&page), Some((b"m".to_vec(), rid(0, 7))));
        let keys: Vec<Vec<u8>> = leaf_entries(&page).map(|(k, _)| k).collect();
        assert_eq!(keys, vec![b"a".to_vec(), b"z".to_vec()]);
        assert_eq!(leaf_next(&page), 9);
    }
}
