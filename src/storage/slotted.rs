use crate::{Error, Result};

use crate::storage::page::PAGE_SIZE;

const HEADER_SIZE: usize = 4;
const SLOT_SIZE: usize = 4;

fn num_slots(page: &[u8]) -> usize {
    u16::from_le_bytes([page[0], page[1]]) as usize
}

fn set_num_slots(page: &mut [u8], n: usize) {
    page[0..2].copy_from_slice(&(n as u16).to_le_bytes());
}

fn raw_free_upper(page: &[u8]) -> usize {
    u16::from_le_bytes([page[2], page[3]]) as usize
}

fn set_free_upper(page: &mut [u8], v: usize) {
    page[2..4].copy_from_slice(&(v as u16).to_le_bytes());
}

fn free_upper(page: &[u8]) -> usize {
    match raw_free_upper(page) {
        0 => PAGE_SIZE,
        v => v,
    }
}

fn get_slot(page: &[u8], i: usize) -> (usize, usize) {
    let base = HEADER_SIZE + i * SLOT_SIZE;
    let off = u16::from_le_bytes([page[base], page[base + 1]]) as usize;
    let len = u16::from_le_bytes([page[base + 2], page[base + 3]]) as usize;
    (off, len)
}

fn set_slot(page: &mut [u8], i: usize, off: usize, len: usize) {
    let base = HEADER_SIZE + i * SLOT_SIZE;
    page[base..base + 2].copy_from_slice(&(off as u16).to_le_bytes());
    page[base + 2..base + 4].copy_from_slice(&(len as u16).to_le_bytes());
}

pub fn page_insert(page: &mut [u8; PAGE_SIZE], record: &[u8]) -> Result<u16> {
    let n = num_slots(page);
    let upper = free_upper(page);
    let reuse = (0..n).find(|&i| get_slot(page, i).0 == 0);
    let dir_end = HEADER_SIZE + (n + if reuse.is_none() { 1 } else { 0 }) * SLOT_SIZE;
    if dir_end + record.len() > upper {
        return Err(Error::PageFull);
    }
    let off = upper - record.len();
    page[off..upper].copy_from_slice(record);
    match reuse {
        Some(i) => {
            set_slot(page, i, off, record.len());
            Ok(i as u16)
        }
        None => {
            set_slot(page, n, off, record.len());
            set_num_slots(page, n + 1);
            Ok(n as u16)
        }
    } 
    .map(|slot| {
        set_free_upper(page, off);
        slot
    })
}

/// Writes `record` at exactly `slot`, extending the slot directory as
/// needed. The target slot must be empty. Used by WAL replay to restore
/// records at their original rids.
pub fn page_put_at(page: &mut [u8; PAGE_SIZE], slot: u16, record: &[u8]) -> Result<()> {
    let slot = slot as usize;
    let n = num_slots(page);
    let upper = free_upper(page);
    if slot < n && get_slot(page, slot) != (0, 0) {
        return Err(Error::Runtime(format!("slot {slot} already occupied")));
    }
    let grow = if slot < n { 0 } else { (slot + 1 - n) * SLOT_SIZE };
    let dir_end = HEADER_SIZE + (slot + 1).max(n) * SLOT_SIZE;
    if dir_end + record.len() > upper {
        return Err(Error::PageFull);
    }
    let off = upper - record.len();
    page[off..upper].copy_from_slice(record);
    if grow > 0 {
        // entries between the old count and the new slot stay empty
        let from = HEADER_SIZE + n * SLOT_SIZE;
        for b in page[from..from + grow].iter_mut() {
            *b = 0;
        }
        set_num_slots(page, slot + 1);
    }
    set_slot(page, slot, off, record.len());
    set_free_upper(page, off);
    Ok(())
}

pub fn page_get<'a>(page: &'a [u8; PAGE_SIZE], slot: u16) -> Result<Option<&'a [u8]>> {
    let n = num_slots(page);
    if slot as usize >= n {
        return Ok(None);
    }
    let (off, len) = get_slot(page, slot as usize);
    if off == 0 && len == 0 {
        return Ok(None);
    }
    Ok(Some(&page[off..off + len]))
}

pub fn page_delete(page: &mut [u8; PAGE_SIZE], slot: u16) -> Result<()> {
    let n = num_slots(page);
    if slot as usize >= n {
        return Err(Error::Runtime(format!("no record at slot {slot}")));
    }
    let (off, len) = get_slot(page, slot as usize);
    if off == 0 && len == 0 {
        return Err(Error::Runtime(format!("no record at slot {slot}")));
    }
    set_slot(page, slot as usize, 0, 0);
    compact(page);
    Ok(())
}

fn compact(page: &mut [u8; PAGE_SIZE]) {
    let n = num_slots(page);
    let mut live: Vec<(usize, Vec<u8>)> = Vec::new();
    for i in 0..n {
        let (off, len) = get_slot(page, i);
        if off != 0 {
            live.push((i, page[off..off + len].to_vec()));
        }
    }
    let mut upper = PAGE_SIZE;
    for (i, data) in &live {
        upper -= data.len();
        page[upper..upper + data.len()].copy_from_slice(data);
        set_slot(page, *i, upper, data.len());
    }
    set_free_upper(page, upper);
}

pub fn page_write(page: &mut [u8; PAGE_SIZE], slot: u16, data: &[u8]) -> Result<()> {
    let n = num_slots(page);
    if slot as usize >= n {
        return Err(Error::Runtime(format!("no record at slot {slot}")));
    }
    let (off, len) = get_slot(page, slot as usize);
    if off == 0 && len == 0 {
        return Err(Error::Runtime(format!("no record at slot {slot}")));
    }
    if data.len() != len {
        return Err(Error::Runtime("record length mismatch on rewrite".into()));
    }
    page[off..off + len].copy_from_slice(data);
    Ok(())
}

pub fn page_iter<'a>(page: &'a [u8; PAGE_SIZE]) -> impl Iterator<Item = (u16, &'a [u8])> + 'a {
    let n = num_slots(page) as u16;
    (0..n).filter_map(move |s| page_get(page, s).ok().flatten().map(|r| (s, r)))
}
