use crate::storage::{PageNo, Rid, PAGE_SIZE};
use crate::{Error, Result};

pub const LEAF: u8 = 0;
pub const INTERNAL: u8 = 1;

const MAX_KEY_LEN: usize = u16::MAX as usize;

// leaf header: type@0, num@1..3, prev@3..7, next@7..11, entries@11
const LEAF_ENTRIES: usize = 11;
const ENTRY_OVERHEAD: usize = 2; // key_len u16
/// Bytes occupied by a leaf page header (before the first entry).
pub const LEAF_HEADER: usize = LEAF_ENTRIES;
/// Encoded size of one leaf entry with a `key_len`-byte key.
pub const fn leaf_entry_size(key_len: usize) -> usize {
    ENTRY_OVERHEAD + key_len + 6
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

pub fn leaf_init(page: &mut [u8; PAGE_SIZE], prev: PageNo, next: PageNo) {
    page[0] = LEAF;
    u16_set(page, 1, 0);
    u32_set(page, 3, prev);
    u32_set(page, 7, next);
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

pub fn leaf_num(page: &[u8]) -> usize {
    u16_at(page, 1)
}

fn leaf_entry_offset(page: &[u8], i: usize) -> usize {
    let mut off = LEAF_ENTRIES;
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

/// Walks the variable-length entries once, comparing raw key bytes. Entries are
/// not fixed-width, so a seek still costs one pass, but it avoids rescanning
/// every earlier entry (and building a `Vec` key) at each step.
fn leaf_bound(page: &[u8], key: &[u8], strict: bool) -> usize {
    let n = leaf_num(page);
    let mut off = LEAF_ENTRIES;
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
    // shift entries [idx..n] right by `need`
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

// internal header: type@0, num@1..3, first_child@3..7, entries@7
const INTERNAL_ENTRIES: usize = 7;
/// Bytes occupied by an internal page header (before the first entry).
pub const INTERNAL_HEADER: usize = INTERNAL_ENTRIES;
/// Encoded size of one internal entry with a `key_len`-byte key.
pub const fn internal_entry_size(key_len: usize) -> usize {
    ENTRY_OVERHEAD + key_len + 4
}

pub fn internal_init(page: &mut [u8; PAGE_SIZE], first_child: PageNo) {
    page[0] = INTERNAL;
    u16_set(page, 1, 0);
    u32_set(page, 3, first_child);
}

pub fn internal_first_child(page: &[u8]) -> PageNo {
    u32_at(page, 3)
}

pub fn internal_set_first_child(page: &mut [u8], child: PageNo) {
    u32_set(page, 3, child);
}

pub fn internal_num(page: &[u8]) -> usize {
    u16_at(page, 1)
}

fn internal_entry_offset(page: &[u8], i: usize) -> usize {
    let mut off = INTERNAL_ENTRIES;
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
    let mut off = INTERNAL_ENTRIES;
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
}
