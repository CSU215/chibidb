//! B-link index node layout.
//!
//! Every node carries a `next` page (its right sibling, the B-link) and a
//! `high_key`: a bound such that any key `>= high_key` belongs to a node to the
//! right. A zero-length high key means "unbounded" (the rightmost node / root).
//! The high key lets a descent move right without re-reading the parent, which
//! is what makes concurrent splits safe.
//!
//! leaf header:     type@0 num@1..3 prev@3..7 next@7..11 hk_len@11..13 hk@13.. entries@(13+hk_len)
//! internal header: type@0 num@1..3 first_child@3..7 next@7..11 hk_len@11..13 hk@13.. entries@(13+hk_len)

use crate::storage::{PageNo, Rid, PAGE_SIZE};
use crate::{Error, Result};

pub const LEAF: u8 = 0;
pub const INTERNAL: u8 = 1;

const MAX_KEY_LEN: usize = u16::MAX as usize;

const LEAF_BASE: usize = 13;
const INTERNAL_BASE: usize = 13;
const ENTRY_OVERHEAD: usize = 2; // key_len u16

/// Encoded size of one leaf entry with a `key_len`-byte key.
pub const fn leaf_entry_size(key_len: usize) -> usize {
    ENTRY_OVERHEAD + key_len + 6
}

/// Encoded size of one internal entry with a `key_len`-byte key.
pub const fn internal_entry_size(key_len: usize) -> usize {
    ENTRY_OVERHEAD + key_len + 4
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

fn hk_len(page: &[u8]) -> usize {
    u16_at(page, 11)
}

fn high_key(page: &[u8]) -> Vec<u8> {
    let l = hk_len(page);
    page[LEAF_BASE..LEAF_BASE + l].to_vec()
}

fn set_high_key(page: &mut [u8; PAGE_SIZE], key: &[u8]) -> Result<()> {
    if key.len() > MAX_KEY_LEN || LEAF_BASE + key.len() > PAGE_SIZE {
        return Err(Error::Runtime("index high key too large".into()));
    }
    u16_set(page, 11, key.len());
    page[LEAF_BASE..LEAF_BASE + key.len()].copy_from_slice(key);
    Ok(())
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

pub fn leaf_high_key(page: &[u8]) -> Vec<u8> {
    high_key(page)
}

/// Sets the leaf's high key. Only valid on an empty leaf (it shifts the entry
/// area); builders set it before inserting entries.
pub fn leaf_set_high_key(page: &mut [u8; PAGE_SIZE], key: &[u8]) -> Result<()> {
    set_high_key(page, key)
}

pub fn leaf_num(page: &[u8]) -> usize {
    u16_at(page, 1)
}

fn leaf_entries_start(page: &[u8]) -> usize {
    LEAF_BASE + hk_len(page)
}

/// Bytes occupied by a leaf page header (before the first entry), for a given
/// encoded high-key length.
pub const fn leaf_header_len(high_key_len: usize) -> usize {
    LEAF_BASE + high_key_len
}

fn leaf_entry_offset(page: &[u8], i: usize) -> usize {
    let mut off = leaf_entries_start(page);
    for _ in 0..i {
        let key_len = u16_at(page, off);
        off += ENTRY_OVERHEAD + key_len + 6; // + page_no u32 + slot u16
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

/// First entry index whose key >= `key`.
pub fn leaf_lower_bound(page: &[u8], key: &[u8]) -> usize {
    leaf_bound(page, key, false)
}

/// First entry index whose key > `key` (insertion point after equal keys).
pub fn leaf_upper_bound(page: &[u8], key: &[u8]) -> usize {
    leaf_bound(page, key, true)
}

fn leaf_bound(page: &[u8], key: &[u8], strict: bool) -> usize {
    let n = leaf_num(page);
    let mut off = leaf_entries_start(page);
    for i in 0..n {
        let key_len = u16_at(page, off);
        let k = &page[off + 2..off + 2 + key_len];
        let past = if strict { k > key } else { k >= key };
        if past {
            return i;
        }
        off += ENTRY_OVERHEAD + key_len + 6;
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
    let need = ENTRY_OVERHEAD + key.len() + 6;
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
    let size = ENTRY_OVERHEAD + key_len + 6;
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

pub fn internal_set_next(page: &mut [u8], next: PageNo) {
    u32_set(page, 7, next);
}

pub fn internal_high_key(page: &[u8]) -> Vec<u8> {
    high_key(page)
}

pub fn internal_set_high_key(page: &mut [u8; PAGE_SIZE], key: &[u8]) -> Result<()> {
    set_high_key(page, key)
}

pub fn internal_num(page: &[u8]) -> usize {
    u16_at(page, 1)
}

fn internal_entries_start(page: &[u8]) -> usize {
    INTERNAL_BASE + hk_len(page)
}

/// Bytes occupied by an internal page header, for a given high-key length.
pub const fn internal_header_len(high_key_len: usize) -> usize {
    INTERNAL_BASE + high_key_len
}

fn internal_entry_offset(page: &[u8], i: usize) -> usize {
    let mut off = internal_entries_start(page);
    for _ in 0..i {
        let key_len = u16_at(page, off);
        off += ENTRY_OVERHEAD + key_len + 4;
    }
    off
}

pub fn internal_entry_at(page: &[u8], i: usize) -> (Vec<u8>, PageNo) {
    let off = internal_entry_offset(page, i);
    let key_len = u16_at(page, off);
    let key = page[off + 2..off + 2 + key_len].to_vec();
    let child = u32_at(page, off + 2 + key_len);
    (key, child)
}

pub fn internal_entries<'a>(page: &'a [u8]) -> impl Iterator<Item = (Vec<u8>, PageNo)> + 'a {
    let n = internal_num(page) as u16;
    (0..n).map(move |i| internal_entry_at(page, i as usize))
}

/// Child page covering `key`: separators route keys `>= sep` rightwards.
pub fn internal_child_for(page: &[u8], key: &[u8]) -> PageNo {
    let n = internal_num(page);
    let mut child = internal_first_child(page);
    let mut off = internal_entries_start(page);
    for _ in 0..n {
        let key_len = u16_at(page, off);
        let sep = &page[off + 2..off + 2 + key_len];
        if key < sep {
            break;
        }
        child = u32_at(page, off + 2 + key_len);
        off += ENTRY_OVERHEAD + key_len + 4;
    }
    child
}

pub fn internal_insert_entry(
    page: &mut [u8; PAGE_SIZE],
    idx: usize,
    key: &[u8],
    child: PageNo,
) -> Result<()> {
    let n = internal_num(page);
    let end = internal_entry_offset(page, n);
    let need = ENTRY_OVERHEAD + key.len() + 4;
    if end + need > PAGE_SIZE {
        return Err(Error::PageFull);
    }
    let at = internal_entry_offset(page, idx);
    page.copy_within(at..end, at + need);
    u16_set(page, at, key.len());
    page[at + 2..at + 2 + key.len()].copy_from_slice(key);
    u32_set(page, at + 2 + key.len(), child);
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
    let size = ENTRY_OVERHEAD + key_len + 4;
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

    fn leaf_with(keys: &[&[u8]]) -> [u8; PAGE_SIZE] {
        let mut page: [u8; PAGE_SIZE] = *zeroed_page();
        leaf_init(&mut page, 0, 0);
        for (i, key) in keys.iter().enumerate() {
            leaf_insert_at(&mut page, i, key, Rid::new(1, i as u16)).unwrap();
        }
        page
    }

    #[test]
    fn lower_and_upper_bound_cover_duplicates_and_edges() {
        let page = leaf_with(&[b"\x01", b"\x03", b"\x03", b"\x03", b"\x07", b"\x09"]);
        assert_eq!(leaf_lower_bound(&page, b"\x00"), 0);
        assert_eq!(leaf_lower_bound(&page, b"\x01"), 0);
        assert_eq!(leaf_lower_bound(&page, b"\x03"), 1);
        assert_eq!(leaf_lower_bound(&page, b"\x04"), 4);
        assert_eq!(leaf_lower_bound(&page, b"\x09"), 5);
        assert_eq!(leaf_lower_bound(&page, b"\x0a"), 6);

        assert_eq!(leaf_upper_bound(&page, b"\x00"), 0);
        assert_eq!(leaf_upper_bound(&page, b"\x01"), 1);
        assert_eq!(leaf_upper_bound(&page, b"\x03"), 4);
        assert_eq!(leaf_upper_bound(&page, b"\x08"), 5);
        assert_eq!(leaf_upper_bound(&page, b"\x09"), 6);
    }

    #[test]
    fn internal_child_for_picks_the_rightmost_covering_separator() {
        let mut page: [u8; PAGE_SIZE] = *zeroed_page();
        internal_init(&mut page, 10);
        internal_insert_entry(&mut page, 0, b"\x03", 11).unwrap();
        internal_insert_entry(&mut page, 1, b"\x05", 12).unwrap();
        assert_eq!(internal_child_for(&page, b"\x00"), 10);
        assert_eq!(internal_child_for(&page, b"\x03"), 11);
        assert_eq!(internal_child_for(&page, b"\x04"), 11);
        assert_eq!(internal_child_for(&page, b"\x05"), 12);
        assert_eq!(internal_child_for(&page, b"\xff"), 12);
    }

    #[test]
    fn a_high_key_shifts_the_entry_area_without_corrupting_entries() {
        let mut page: [u8; PAGE_SIZE] = *zeroed_page();
        leaf_init(&mut page, 0, 9);
        leaf_set_high_key(&mut page, b"m").unwrap();
        leaf_insert_at(&mut page, 0, b"a", Rid::new(1, 1)).unwrap();
        leaf_insert_at(&mut page, 1, b"z", Rid::new(2, 2)).unwrap();
        assert_eq!(leaf_high_key(&page), b"m");
        assert_eq!(leaf_chain(&page), vec![b"a".to_vec(), b"z".to_vec()]);
        assert_eq!(leaf_next(&page), 9);
    }

    fn leaf_chain(page: &[u8]) -> Vec<Vec<u8>> {
        leaf_entries(page).map(|(k, _)| k).collect()
    }
}
