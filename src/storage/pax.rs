//! PAX page layout: one page is a column-major row group.
//!
//! A record keeps its exact versioned bytes logically, but a PAX page stores
//! the two MVCC fields in a fixed version segment and each column's tagged
//! values in its own slotted segment. A scan that reads one column never
//! touches the others, so wide rows stop costing the whole row width.
//!
//! Page header (all little-endian):
//! ```text
//! [0..4)   magic "PAX2"
//! [4..6)   nrows   slot high-water mark (slots are never renumbered)
//! [6..8)   ncols
//! [8..10)  nseg    ncols + 1 (segment 0 is the version segment)
//! [10..12) used    packed region end
//! [12..16) reserved
//! [16..16+nseg*4) segment directory of (offset u16, length u16)
//! ```
//! Segment 0 is `nrows * 16` version bytes (`creator`, `deleter` as `u64`). Segment
//! `1+c` holds column `c`: `(nrows+1)` `u16` offsets relative to the value
//! data, then the tagged values. A slot is empty when its column-0 value has
//! zero length, which also keeps empty slots' space reclaimable.

use crate::storage::codec::row_column_ranges;
use crate::storage::page::PAGE_SIZE;
use crate::{Error, Result};

const MAGIC: [u8; 4] = *b"PAX2";

/// Size in bytes of the `(creator, deleter)` version pair stored per slot.
const VERSION_LEN: usize = 16;
const HEADER: usize = 16;
const DIR_ENTRY: usize = 4;

/// Whether `page` has been formatted as a PAX page. A freshly allocated
/// (zeroed) page is not, so the first insert picks the column count.
pub(crate) fn is_pax(page: &[u8]) -> bool {
    page.len() >= 4 && page[0..4] == MAGIC
}

fn nrows(page: &[u8]) -> usize {
    u16::from_le_bytes([page[4], page[5]]) as usize
}

fn ncols(page: &[u8]) -> usize {
    u16::from_le_bytes([page[6], page[7]]) as usize
}

fn dir(page: &[u8], i: usize) -> (usize, usize) {
    let base = HEADER + i * DIR_ENTRY;
    let off = u16::from_le_bytes([page[base], page[base + 1]]) as usize;
    let len = u16::from_le_bytes([page[base + 2], page[base + 3]]) as usize;
    (off, len)
}

fn set_dir(page: &mut [u8], i: usize, off: usize, len: usize) {
    let base = HEADER + i * DIR_ENTRY;
    page[base..base + 2].copy_from_slice(&(off as u16).to_le_bytes());
    page[base + 2..base + 4].copy_from_slice(&(len as u16).to_le_bytes());
}

/// `(offsets offset, value-data offset)` of column `c`'s segment.
fn col_region(page: &[u8], c: usize) -> (usize, usize) {
    let (off, _) = dir(page, 1 + c);
    (off, off + 2 * (nrows(page) + 1))
}

fn col_value(page: &[u8], c: usize, slot: usize) -> &[u8] {
    let (offs, data) = col_region(page, c);
    let a = u16::from_le_bytes([page[offs + 2 * slot], page[offs + 2 * slot + 1]]) as usize;
    let b = u16::from_le_bytes([page[offs + 2 * slot + 2], page[offs + 2 * slot + 3]]) as usize;
    &page[data + a..data + b]
}

/// A slot with no row. Fresh pages (no column segments) are entirely empty.
pub(crate) fn is_empty(page: &[u8], slot: u16) -> bool {
    if !is_pax(page) || ncols(page) == 0 {
        return true;
    }
    slot as usize >= nrows(page) || col_value(page, 0, slot as usize).is_empty()
}

/// Number of row slots the page has ever held; slots are never renumbered.
pub(crate) fn slot_count(page: &[u8]) -> usize {
    if is_pax(page) { nrows(page) } else { 0 }
}

/// Slot numbers of every live row, in order.
pub(crate) fn alive_slots(page: &[u8]) -> impl Iterator<Item = u16> + '_ {
    (0..slot_count(page)).filter_map(move |s| (!is_empty(page, s as u16)).then_some(s as u16))
}

/// `(creator, deleter)` of a live slot's version.
pub(crate) fn version_at(page: &[u8], slot: u16) -> (u64, u64) {
    let (voff, _) = dir(page, 0);
    let at = voff + slot as usize * VERSION_LEN;
    (
        u64::from_le_bytes(page[at..at + 8].try_into().unwrap()),
        u64::from_le_bytes(page[at + 8..at + 16].try_into().unwrap()),
    )
}

/// Tagged value bytes of column `col` in a slot; empty when the slot is empty.
pub(crate) fn column_bytes(page: &[u8], col: usize, slot: u16) -> &[u8] {
    if is_empty(page, slot) {
        &[]
    } else {
        col_value(page, col, slot as usize)
    }
}

/// Writes a fresh page from `nrows` slots and `ncols` columns. `ver(slot)`
/// yields the 16 version bytes; `val(slot, col)` yields a column's tagged value
/// bytes (`&[]` for an empty slot).
fn pack<'a, FV, FC>(
    dst: &mut [u8; PAGE_SIZE],
    nrows: usize,
    ncols: usize,
    ver: FV,
    val: FC,
) -> Result<()>
where
    FV: Fn(usize) -> [u8; 16],
    FC: Fn(usize, usize) -> &'a [u8],
{
    let nseg = ncols + 1;
    let dir_end = HEADER + nseg * DIR_ENTRY;
    let mut total = dir_end + nrows * VERSION_LEN;
    for c in 0..ncols {
        total += 2 * (nrows + 1);
        for s in 0..nrows {
            total += val(s, c).len();
        }
    }
    if total > PAGE_SIZE {
        return Err(Error::PageFull);
    }
    dst.fill(0);
    dst[0..4].copy_from_slice(&MAGIC);
    dst[4..6].copy_from_slice(&(nrows as u16).to_le_bytes());
    dst[6..8].copy_from_slice(&(ncols as u16).to_le_bytes());
    dst[8..10].copy_from_slice(&(nseg as u16).to_le_bytes());
    dst[10..12].copy_from_slice(&(total as u16).to_le_bytes());

    let mut cur = dir_end;
    set_dir(dst, 0, cur, nrows * VERSION_LEN);
    for s in 0..nrows {
        let at = cur + s * VERSION_LEN;
        dst[at..at + VERSION_LEN].copy_from_slice(&ver(s));
    }
    cur += nrows * VERSION_LEN;

    for c in 0..ncols {
        let seg_off = cur;
        let mut data_off = seg_off + 2 * (nrows + 1);
        let mut rel = 0u16;
        for s in 0..nrows {
            dst[seg_off + 2 * s..seg_off + 2 * s + 2].copy_from_slice(&rel.to_le_bytes());
            let v = val(s, c);
            if !v.is_empty() {
                dst[data_off..data_off + v.len()].copy_from_slice(v);
                data_off += v.len();
                rel += v.len() as u16;
            }
        }
        dst[seg_off + 2 * nrows..seg_off + 2 * nrows + 2].copy_from_slice(&rel.to_le_bytes());
        set_dir(dst, 1 + c, seg_off, data_off - seg_off);
        cur = data_off;
    }
    Ok(())
}

/// Byte ranges of a row's columns, relative to the row encoding.
type ColumnRanges = Vec<(usize, usize)>;

/// Splits a versioned record into `(version bytes, value ranges)`.
fn split(record: &[u8]) -> Result<([u8; 16], ColumnRanges)> {
    if record.len() < VERSION_LEN {
        return Err(Error::Runtime("truncated versioned record".into()));
    }
    let version = record[0..VERSION_LEN].try_into().unwrap();
    let ranges = row_column_ranges(&record[VERSION_LEN..])?;
    Ok((version, ranges))
}

/// Appends (or reuses an empty slot for) a versioned record.
pub(crate) fn insert(page: &mut [u8; PAGE_SIZE], record: &[u8]) -> Result<u16> {
    let (_version, ranges) = split(record)?;
    let count = ranges.len();
    let src = *page;
    let had = is_pax(&src);
    if had && count != ncols(&src) {
        return Err(Error::Runtime("pax page column count mismatch".into()));
    }
    let ncols = if had { ncols(&src) } else { count };
    let old_n = if had { nrows(&src) } else { 0 };
    let slot = (0..old_n)
        .find(|&s| is_empty(&src, s as u16))
        .unwrap_or(old_n);
    let n = old_n.max(slot + 1);
    let row = &record[VERSION_LEN..];
    let voff = if had { dir(&src, 0).0 } else { 0 };

    let ver = |s: usize| -> [u8; 16] {
        if s == slot {
            record[0..VERSION_LEN].try_into().unwrap()
        } else if s < old_n {
            src[voff + s * VERSION_LEN..voff + (s + 1) * VERSION_LEN]
                .try_into()
                .unwrap()
        } else {
            [0u8; VERSION_LEN]
        }
    };
    let val = |s: usize, c: usize| -> &[u8] {
        if s == slot {
            &row[ranges[c].0..ranges[c].1]
        } else if s < old_n {
            col_value(&src, c, s)
        } else {
            &[]
        }
    };
    pack(page, n, ncols, ver, val)?;
    Ok(slot as u16)
}

/// Places an already-encoded record at exactly `slot` (WAL replay), extending
/// the slot range with empty slots as needed. The slot must be empty.
pub(crate) fn put_at(page: &mut [u8; PAGE_SIZE], slot: u16, record: &[u8]) -> Result<()> {
    let (_version, ranges) = split(record)?;
    let count = ranges.len();
    let src = *page;
    let had = is_pax(&src);
    if had && count != ncols(&src) {
        return Err(Error::Runtime("pax page column count mismatch".into()));
    }
    if !is_empty(&src, slot) {
        return Err(Error::Runtime(format!("slot {slot} already occupied")));
    }
    let ncols = if had { ncols(&src) } else { count };
    let old_n = if had { nrows(&src) } else { 0 };
    let target = slot as usize;
    let n = old_n.max(target + 1);
    let row = &record[VERSION_LEN..];
    let voff = if had { dir(&src, 0).0 } else { 0 };

    let ver = |s: usize| -> [u8; 16] {
        if s == target {
            record[0..VERSION_LEN].try_into().unwrap()
        } else if s < old_n {
            src[voff + s * VERSION_LEN..voff + (s + 1) * VERSION_LEN]
                .try_into()
                .unwrap()
        } else {
            [0u8; VERSION_LEN]
        }
    };
    let val = |s: usize, c: usize| -> &[u8] {
        if s == target {
            &row[ranges[c].0..ranges[c].1]
        } else if s < old_n {
            col_value(&src, c, s)
        } else {
            &[]
        }
    };
    pack(page, n, ncols, ver, val)
}

/// Marks a slot empty, reclaiming its value bytes but keeping its index.
pub(crate) fn delete(page: &mut [u8; PAGE_SIZE], slot: u16) -> Result<()> {
    if is_empty(page, slot) {
        return Err(Error::Runtime(format!("no record at slot {slot}")));
    }
    let src = *page;
    let n = nrows(&src);
    let ncols = ncols(&src);
    let target = slot as usize;
    let voff = dir(&src, 0).0;
    let ver = |s: usize| -> [u8; 16] {
        if s == target {
            [0u8; VERSION_LEN]
        } else {
            src[voff + s * VERSION_LEN..voff + (s + 1) * VERSION_LEN]
                .try_into()
                .unwrap()
        }
    };
    let val = |s: usize, c: usize| -> &[u8] {
        if s == target { &[] } else { col_value(&src, c, s) }
    };
    pack(page, n, ncols, ver, val)
}

/// Rewrites the deleter field in place, returning the previous value.
pub(crate) fn delete_mark(page: &mut [u8; PAGE_SIZE], slot: u16, deleter: u64) -> Result<u64> {
    if is_empty(page, slot) {
        return Err(Error::Runtime(format!("no record at slot {slot}")));
    }
    let (voff, _) = dir(page, 0);
    let at = voff + slot as usize * VERSION_LEN;
    let prev = u64::from_le_bytes(page[at + 8..at + 16].try_into().unwrap());
    page[at + 8..at + 16].copy_from_slice(&deleter.to_le_bytes());
    Ok(prev)
}

/// Reconstructs a record's versioned bytes. With `keep`, unread columns are
/// written as `NULL` and their value bytes are never touched.
pub(crate) fn read_record(page: &[u8], slot: u16, keep: Option<&[bool]>, out: &mut Vec<u8>) -> bool {
    if is_empty(page, slot) {
        return false;
    }
    let s = slot as usize;
    let ncols = ncols(page);
    let (voff, _) = dir(page, 0);
    out.clear();
    out.extend_from_slice(&page[voff + s * VERSION_LEN..voff + (s + 1) * VERSION_LEN]);
    out.extend_from_slice(&(ncols as u16).to_le_bytes());
    for c in 0..ncols {
        let needed = keep.is_none_or(|k| k.get(c).copied().unwrap_or(true));
        if needed {
            out.extend_from_slice(col_value(page, c, s));
        } else {
            out.push(0x00);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::codec::{decode_row, encode_record_inline, record_version};
    use crate::value::Value;

    fn rec(creator: u64, deleter: u64, vals: &[Value]) -> Vec<u8> {
        encode_record_inline(creator, deleter, vals)
    }

    #[test]
    fn insert_and_read_roundtrip() {
        let mut page = [0u8; PAGE_SIZE];
        assert!(!is_pax(&page));
        let r0 = rec(1, 0, &[Value::Int(7), Value::Str("alice".into())]);
        let r1 = rec(2, 0, &[Value::Int(8), Value::Str("a-much-longer-name".into())]);
        let s0 = insert(&mut page, &r0).unwrap();
        let s1 = insert(&mut page, &r1).unwrap();
        assert_eq!((s0, s1), (0, 1));
        assert_eq!(slot_count(&page), 2);
        let mut out = Vec::new();
        assert!(read_record(&page, s0, None, &mut out));
        assert_eq!(out, r0);
        assert!(read_record(&page, s1, None, &mut out));
        assert_eq!(out, r1);
        assert_eq!(record_version(&out).unwrap().0, 2);
    }

    #[test]
    fn delete_frees_the_slot_for_reuse() {
        let mut page = [0u8; PAGE_SIZE];
        let s0 = insert(&mut page, &rec(1, 0, &[Value::Int(1)])).unwrap();
        let s1 = insert(&mut page, &rec(2, 0, &[Value::Int(2)])).unwrap();
        delete(&mut page, s0).unwrap();
        let mut out = Vec::new();
        assert!(!read_record(&page, s0, None, &mut out));
        assert!(read_record(&page, s1, None, &mut out));
        // the freed slot is reused, the live one is untouched
        let s2 = insert(&mut page, &rec(3, 0, &[Value::Int(3)])).unwrap();
        assert_eq!(s2, s0);
        assert!(read_record(&page, s2, None, &mut out));
        assert_eq!(record_version(&out).unwrap().0, 3);
    }

    #[test]
    fn delete_mark_updates_deleter_in_place() {
        let mut page = [0u8; PAGE_SIZE];
        let r = rec(5, 0, &[Value::Int(1), Value::Str("x".into())]);
        let s = insert(&mut page, &r).unwrap();
        assert_eq!(delete_mark(&mut page, s, 9).unwrap(), 0);
        assert_eq!(delete_mark(&mut page, s, 11).unwrap(), 9);
        let mut out = Vec::new();
        assert!(read_record(&page, s, None, &mut out));
        assert_eq!(out, rec(5, 11, &[Value::Int(1), Value::Str("x".into())]));
    }

    #[test]
    fn projection_nulls_unread_columns() {
        let mut page = [0u8; PAGE_SIZE];
        let r = rec(1, 0, &[Value::Int(7), Value::Str("big".into()), Value::Int(9)]);
        let s = insert(&mut page, &r).unwrap();
        let keep = [false, false, true];
        let mut out = Vec::new();
        assert!(read_record(&page, s, Some(&keep), &mut out));
        let (row, _) = decode_row(&out[VERSION_LEN..]).unwrap();
        assert_eq!(row, vec![Value::Null, Value::Null, Value::Int(9)]);
        // reading all columns is unaffected
        assert!(read_record(&page, s, None, &mut out));
        assert_eq!(out, r);
    }

    #[test]
    fn put_at_extends_with_empty_slots() {
        let mut page = [0u8; PAGE_SIZE];
        let r = rec(1, 0, &[Value::Int(1), Value::Int(2)]);
        put_at(&mut page, 3, &r).unwrap();
        assert_eq!(slot_count(&page), 4);
        let mut out = Vec::new();
        assert!(!read_record(&page, 0, None, &mut out));
        assert!(!read_record(&page, 2, None, &mut out));
        assert!(read_record(&page, 3, None, &mut out));
        assert_eq!(out, r);
        assert!(put_at(&mut page, 3, &r).is_err(), "slot must be empty");
    }

    #[test]
    fn rejects_a_column_count_mismatch() {
        let mut page = [0u8; PAGE_SIZE];
        insert(&mut page, &rec(1, 0, &[Value::Int(1), Value::Int(2)])).unwrap();
        let err = insert(&mut page, &rec(1, 0, &[Value::Int(1)])).unwrap_err();
        assert!(err.to_string().contains("column count"), "{err}");
    }

    #[test]
    fn reports_page_full() {
        let mut page = [0u8; PAGE_SIZE];
        let r = rec(1, 0, &[Value::Str("x".repeat(100)), Value::Int(1)]);
        let mut n = 0;
        loop {
            match insert(&mut page, &r) {
                Ok(_) => n += 1,
                Err(Error::PageFull) => break,
                Err(e) => panic!("{e}"),
            }
            if n > 1000 {
                panic!("page never filled");
            }
        }
        assert!(n > 20, "only {n} rows fit");
    }
}
