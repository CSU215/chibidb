//! Write-ahead log: append-only redo records that make committed
//! transactions durable even when dirty buffer-pool pages are lost.
//!
//! Frame layout: `[u32 len][u8 type][u64 trx_id][payload]` where `len`
//! counts everything after the length field itself. Recovery replays only
//! transactions that have a commit record; truncated tails and unknown
//! frame types end the scan.

use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use parking_lot::Mutex;

use crate::storage::Rid;
use crate::{Error, Result};

const REC_INSERT: u8 = 1;
const REC_DELETE_MARK: u8 = 2;
const REC_COMMIT: u8 = 3;

/// Header overhead: type byte + trx id.
const HEADER_LEN: usize = 9;

/// One redo record.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    /// A full versioned row image written at an exact rid.
    Insert { file_no: u32, rid: Rid, record: Vec<u8> },
    /// MVCC delete mark (deleter trx id + forward pointer) at an exact rid.
    DeleteMark { file_no: u32, rid: Rid, deleter: u64, next_rid: u64 },
    /// Commit boundary; redo replays only transactions with one of these.
    Commit,
}

pub struct Wal {
    file: Mutex<std::fs::File>,
}

impl Wal {
    pub fn open(path: &Path) -> Result<Self> {
        // a plain write handle, not append-only: Windows denies set_len
        // (checkpoint truncation) on append-mode files
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false) // never destroy an existing log on open
            .open(path)
            .map_err(|e| Error::Runtime(format!("cannot open wal {}: {e}", path.display())))?;
        Ok(Self { file: Mutex::new(file) })
    }

    /// Appends a frame at the end of the log. The file lock makes the
    /// seek+write atomic, so concurrent committers never interleave bytes.
    pub fn append(&self, trx_id: u64, rec: &Record) -> Result<()> {
        let mut file = self.file.lock();
        file.seek(SeekFrom::End(0)).map_err(wal_io)?;
        file.write_all(&encode_frame(trx_id, rec)).map_err(wal_io)
    }

    /// Appends a batch of pre-encoded frames in one write, so a statement's
    /// rows do not each pay a separate syscall.
    pub fn append_frames(&self, frames: &[u8]) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        let mut file = self.file.lock();
        file.seek(SeekFrom::End(0)).map_err(wal_io)?;
        file.write_all(frames).map_err(wal_io)
    }

    /// Durability point: every record appended so far survives a crash.
    /// Because appends are serialized, one `sync` flushes all writers that
    /// raced ahead of it (a simple group commit).
    pub fn sync(&self) -> Result<()> {
        self.file.lock().sync_all().map_err(wal_io)
    }

    /// Current log size in bytes (for budget checks).
    pub fn len(&self) -> Result<u64> {
        self.file.lock().metadata().map(|m| m.len()).map_err(wal_io)
    }

    /// True when the log holds no frames.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Checkpoint: only call this after all data pages reached the disk.
    pub fn truncate(&self) -> Result<()> {
        let mut file = self.file.lock();
        file.set_len(0).map_err(wal_io)?;
        file.seek(SeekFrom::Start(0)).map_err(wal_io)?;
        file.sync_all().map_err(wal_io)?;
        Ok(())
    }
}

fn wal_io(e: std::io::Error) -> Error {
    Error::Runtime(format!("wal io error: {e}"))
}

pub fn encode_frame(trx_id: u64, rec: &Record) -> Vec<u8> {
    let mut frame = Vec::new();
    encode_frame_into(&mut frame, trx_id, rec);
    frame
}

/// Appends one encoded frame to `out`, so a transaction can buffer its rows
/// and flush them in a single write at commit.
pub fn encode_frame_into(out: &mut Vec<u8>, trx_id: u64, rec: &Record) {
    let start = out.len();
    out.extend_from_slice(&[0u8; 4]); // length, patched once the frame is written
    match rec {
        Record::Insert { file_no, rid, record } => {
            out.push(REC_INSERT);
            out.extend_from_slice(&trx_id.to_le_bytes());
            out.extend_from_slice(&file_no.to_le_bytes());
            out.extend_from_slice(&rid.page_no.to_le_bytes());
            out.extend_from_slice(&rid.slot.to_le_bytes());
            out.extend_from_slice(record);
        }
        Record::DeleteMark { file_no, rid, deleter, next_rid } => {
            out.push(REC_DELETE_MARK);
            out.extend_from_slice(&trx_id.to_le_bytes());
            out.extend_from_slice(&file_no.to_le_bytes());
            out.extend_from_slice(&rid.page_no.to_le_bytes());
            out.extend_from_slice(&rid.slot.to_le_bytes());
            out.extend_from_slice(&deleter.to_le_bytes());
            out.extend_from_slice(&next_rid.to_le_bytes());
        }
        Record::Commit => {
            out.push(REC_COMMIT);
            out.extend_from_slice(&trx_id.to_le_bytes());
        }
    }
    // `len` counts everything after the length field, as `decode_frame` expects.
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

/// What recovery should do with a log image.
#[derive(Debug, Default, PartialEq)]
pub struct RecoveryPlan {
    /// (commit position, trx id, redo records in log order), sorted by the
    /// commit position so replay follows the original commit order.
    pub committed: Vec<(u64, u64, Vec<Record>)>,
    /// Every trx id with a commit record, used to repair the catalog's
    /// committed set.
    pub committed_ids: Vec<u64>,
    /// Highest trx id seen anywhere; uncommitted ids must never be reused.
    pub max_trx_id: u64,
}

/// Parses a raw log image. Stops at the first truncated or unknown frame.
pub fn plan_recovery(bytes: &[u8]) -> RecoveryPlan {
    let mut records: BTreeMap<u64, Vec<Record>> = BTreeMap::new();
    let mut commit_pos: BTreeMap<u64, u64> = BTreeMap::new();
    let mut max_trx_id = 0;
    let mut pos = 0usize;
    while let Some((trx_id, rec, frame_len)) = decode_frame(&bytes[pos..]) {
        pos += frame_len;
        max_trx_id = max_trx_id.max(trx_id);
        match rec {
            Record::Commit => {
                commit_pos.entry(trx_id).or_insert(pos as u64);
            }
            other => {
                records.entry(trx_id).or_default().push(other);
            }
        }
    }
    let mut committed: Vec<(u64, u64, Vec<Record>)> = commit_pos
        .iter()
        .filter_map(|(trx, pos)| {
            records.remove(trx).map(|recs| (*pos, *trx, recs))
        })
        .collect();
    committed.sort_by_key(|(pos, _, _)| *pos);
    RecoveryPlan { committed, committed_ids: commit_pos.into_keys().collect(), max_trx_id }
}

/// Returns (trx id, record, frame length) or None at end-of-log / corruption.
fn decode_frame(bytes: &[u8]) -> Option<(u64, Record, usize)> {
    if bytes.len() < 4 {
        return None;
    }
    let len = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    if len < HEADER_LEN || bytes.len() < 4 + len {
        return None; // truncated tail frame
    }
    let ty = bytes[4];
    let trx_id = u64::from_le_bytes(bytes[5..13].try_into().unwrap());
    let payload = &bytes[13..4 + len];
    let rec = match ty {
        REC_INSERT => {
            if payload.len() < 10 {
                return None;
            }
            let file_no = u32::from_le_bytes(payload[0..4].try_into().unwrap());
            let page_no = u32::from_le_bytes(payload[4..8].try_into().unwrap());
            let slot = u16::from_le_bytes(payload[8..10].try_into().unwrap());
            Record::Insert { file_no, rid: Rid::new(page_no, slot), record: payload[10..].to_vec() }
        }
        REC_DELETE_MARK => {
            if payload.len() < 26 {
                return None;
            }
            let file_no = u32::from_le_bytes(payload[0..4].try_into().unwrap());
            let page_no = u32::from_le_bytes(payload[4..8].try_into().unwrap());
            let slot = u16::from_le_bytes(payload[8..10].try_into().unwrap());
            let deleter = u64::from_le_bytes(payload[10..18].try_into().unwrap());
            let next_rid = u64::from_le_bytes(payload[18..26].try_into().unwrap());
            Record::DeleteMark { file_no, rid: Rid::new(page_no, slot), deleter, next_rid }
        }
        REC_COMMIT => Record::Commit,
        _ => return None, // unknown frame type: stop scanning
    };
    Some((trx_id, rec, 4 + len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid(page_no: u32, slot: u16) -> Rid {
        Rid::new(page_no, slot)
    }

    #[test]
    fn frames_roundtrip() {
        let recs = [
            (7u64, Record::Insert { file_no: 3, rid: rid(9, 4), record: vec![1, 2, 3] }),
            (8u64, Record::DeleteMark { file_no: 3, rid: rid(9, 4), deleter: 8, next_rid: 0 }),
            (9u64, Record::Commit),
        ];
        let mut bytes = Vec::new();
        for (trx, rec) in &recs {
            bytes.extend(encode_frame(*trx, rec));
        }
        let plan = plan_recovery(&bytes);
        assert_eq!(plan.max_trx_id, 9);
        assert_eq!(plan.committed_ids, vec![9]);
        // trx 7 and 8 never committed: no replay. trx 9 has a commit but no
        // data records: nothing to redo either.
        assert!(plan.committed.is_empty());
    }

    #[test]
    fn buffered_frames_match_individual_encodes() {
        let recs = [
            (7u64, Record::Insert { file_no: 3, rid: rid(9, 4), record: vec![1, 2, 3] }),
            (8u64, Record::DeleteMark { file_no: 3, rid: rid(9, 4), deleter: 8, next_rid: 0 }),
            (8u64, Record::Commit),
        ];
        let mut buffered = Vec::new();
        for (trx, rec) in &recs {
            encode_frame_into(&mut buffered, *trx, rec);
        }
        let mut one_by_one = Vec::new();
        for (trx, rec) in &recs {
            one_by_one.extend_from_slice(&encode_frame(*trx, rec));
        }
        assert_eq!(buffered, one_by_one);
    }

    #[test]
    fn truncated_tail_is_ignored() {
        let mut bytes = encode_frame(1, &Record::Commit);
        bytes.extend_from_slice(&500u32.to_le_bytes()); // promises 500 bytes
        bytes.extend_from_slice(b"tw"); // but delivers two
        let plan = plan_recovery(&bytes);
        assert_eq!(plan.committed_ids, vec![1]);
    }

    #[test]
    fn unknown_frame_type_stops_scan() {
        let mut bytes = encode_frame(1, &Record::Commit);
        bytes[4] = 0xFF;
        let plan = plan_recovery(&bytes);
        assert_eq!(plan.committed_ids, Vec::<u64>::new());
    }

    #[test]
    fn plan_groups_by_trx_in_log_order() {
        let mut bytes = Vec::new();
        bytes.extend(encode_frame(1, &Record::Insert { file_no: 0, rid: rid(1, 0), record: b"a".to_vec() }));
        bytes.extend(encode_frame(2, &Record::Insert { file_no: 0, rid: rid(1, 1), record: b"b".to_vec() }));
        bytes.extend(encode_frame(1, &Record::DeleteMark { file_no: 0, rid: rid(1, 1), deleter: 1, next_rid: 0 }));
        bytes.extend(encode_frame(1, &Record::Commit));
        bytes.extend(encode_frame(2, &Record::Commit));
        let plan = plan_recovery(&bytes);
        assert_eq!(plan.committed_ids, vec![1, 2]);
        assert_eq!(plan.committed.len(), 2);
        let (pos1, trx1, recs1) = &plan.committed[0];
        assert_eq!(*trx1, 1);
        assert_eq!(recs1.len(), 2);
        assert!(matches!(recs1[0], Record::Insert { .. }));
        assert!(matches!(recs1[1], Record::DeleteMark { .. }));
        let (pos2, ..) = plan.committed[1];
        assert!(*pos1 < pos2);
        assert_eq!(plan.max_trx_id, 2);
    }

    #[test]
    fn concurrent_appends_are_framed_and_recoverable() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let wal = Arc::new(Wal::open(&dir.path().join("wal.bin")).unwrap());
        let mut handles = Vec::new();
        for trx in 1..=8u64 {
            let wal = Arc::clone(&wal);
            handles.push(std::thread::spawn(move || {
                for slot in 0..50u16 {
                    wal.append(
                        trx,
                        &Record::Insert {
                            file_no: 0,
                            rid: Rid::new(1, slot),
                            record: vec![trx as u8],
                        },
                    )
                    .unwrap();
                }
                wal.append(trx, &Record::Commit).unwrap();
                wal.sync().unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let bytes = std::fs::read(dir.path().join("wal.bin")).unwrap();
        let plan = plan_recovery(&bytes);
        assert_eq!(plan.committed_ids.len(), 8);
        assert_eq!(plan.committed.len(), 8);
        assert_eq!(plan.max_trx_id, 8);
        for (_, _, recs) in &plan.committed {
            assert_eq!(recs.len(), 50, "every transaction's frames stay intact");
        }
    }

    #[test]
    fn uncommitted_records_are_not_replayed() {
        let mut bytes = Vec::new();
        bytes.extend(encode_frame(5, &Record::Insert { file_no: 0, rid: rid(1, 0), record: b"x".to_vec() }));
        let plan = plan_recovery(&bytes);
        assert!(plan.committed.is_empty());
        assert!(plan.committed_ids.is_empty());
        assert_eq!(plan.max_trx_id, 5);
    }
}
