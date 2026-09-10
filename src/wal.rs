//! Write-ahead log: append-only redo records that make committed
//! transactions durable even when dirty buffer-pool pages are lost.
//!
//! Frame layout: `[u32 len][u8 type][u32 trx_id][payload]` where `len`
//! counts everything after the length field itself. Recovery replays only
//! transactions that have a commit record; truncated tails and unknown
//! frame types end the scan.

use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use crate::storage::Rid;
use crate::{Error, Result};

const REC_INSERT: u8 = 1;
const REC_DELETE_MARK: u8 = 2;
const REC_COMMIT: u8 = 3;

/// Header overhead: type byte + trx id.
const HEADER_LEN: usize = 5;

/// One redo record.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    /// A full versioned row image written at an exact rid.
    Insert { file_no: u32, rid: Rid, record: Vec<u8> },
    /// MVCC delete mark (deleter trx id) on the record at an exact rid.
    DeleteMark { file_no: u32, rid: Rid, deleter: u32 },
    /// Commit boundary; redo replays only transactions with one of these.
    Commit,
}

pub struct Wal {
    file: std::fs::File,
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
        Ok(Self { file })
    }

    pub fn append(&mut self, trx_id: u32, rec: &Record) -> Result<()> {
        self.file.seek(SeekFrom::End(0)).map_err(wal_io)?;
        self.file.write_all(&encode_frame(trx_id, rec)).map_err(wal_io)
    }

    /// Durability point: every record appended so far survives a crash.
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_all().map_err(wal_io)
    }

    /// Current log size in bytes (for budget checks).
    pub fn len(&self) -> Result<u64> {
        self.file.metadata().map(|m| m.len()).map_err(wal_io)
    }

    /// Checkpoint: only call this after all data pages reached the disk.
    pub fn truncate(&mut self) -> Result<()> {
        self.file.set_len(0).map_err(wal_io)?;
        self.file.seek(SeekFrom::Start(0)).map_err(wal_io)?;
        Ok(())
    }
}

fn wal_io(e: std::io::Error) -> Error {
    Error::Runtime(format!("wal io error: {e}"))
}

pub fn encode_frame(trx_id: u32, rec: &Record) -> Vec<u8> {
    let (ty, payload) = match rec {
        Record::Insert { file_no, rid, record } => {
            let mut p = Vec::with_capacity(10 + record.len());
            p.extend_from_slice(&file_no.to_le_bytes());
            p.extend_from_slice(&rid.page_no.to_le_bytes());
            p.extend_from_slice(&rid.slot.to_le_bytes());
            p.extend_from_slice(record);
            (REC_INSERT, p)
        }
        Record::DeleteMark { file_no, rid, deleter } => {
            let mut p = Vec::with_capacity(14);
            p.extend_from_slice(&file_no.to_le_bytes());
            p.extend_from_slice(&rid.page_no.to_le_bytes());
            p.extend_from_slice(&rid.slot.to_le_bytes());
            p.extend_from_slice(&deleter.to_le_bytes());
            (REC_DELETE_MARK, p)
        }
        Record::Commit => (REC_COMMIT, Vec::new()),
    };
    let mut frame = Vec::with_capacity(payload.len() + 9);
    frame.extend_from_slice(&((payload.len() + HEADER_LEN) as u32).to_le_bytes());
    frame.push(ty);
    frame.extend_from_slice(&trx_id.to_le_bytes());
    frame.extend_from_slice(&payload);
    frame
}

/// What recovery should do with a log image.
#[derive(Debug, Default, PartialEq)]
pub struct RecoveryPlan {
    /// (commit position, trx id, redo records in log order), sorted by the
    /// commit position so replay follows the original commit order.
    pub committed: Vec<(u64, u32, Vec<Record>)>,
    /// Every trx id with a commit record, used to repair the catalog's
    /// committed set.
    pub committed_ids: Vec<u32>,
    /// Highest trx id seen anywhere; uncommitted ids must never be reused.
    pub max_trx_id: u32,
}

/// Parses a raw log image. Stops at the first truncated or unknown frame.
pub fn plan_recovery(bytes: &[u8]) -> RecoveryPlan {
    let mut records: BTreeMap<u32, Vec<Record>> = BTreeMap::new();
    let mut commit_pos: BTreeMap<u32, u64> = BTreeMap::new();
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
    let mut committed: Vec<(u64, u32, Vec<Record>)> = commit_pos
        .iter()
        .filter_map(|(trx, pos)| {
            records.remove(trx).map(|recs| (*pos, *trx, recs))
        })
        .collect();
    committed.sort_by_key(|(pos, _, _)| *pos);
    RecoveryPlan { committed, committed_ids: commit_pos.into_keys().collect(), max_trx_id }
}

/// Returns (trx id, record, frame length) or None at end-of-log / corruption.
fn decode_frame(bytes: &[u8]) -> Option<(u32, Record, usize)> {
    if bytes.len() < 4 {
        return None;
    }
    let len = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    if len < HEADER_LEN || bytes.len() < 4 + len {
        return None; // truncated tail frame
    }
    let ty = bytes[4];
    let trx_id = u32::from_le_bytes(bytes[5..9].try_into().unwrap());
    let payload = &bytes[9..4 + len];
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
            if payload.len() < 14 {
                return None;
            }
            let file_no = u32::from_le_bytes(payload[0..4].try_into().unwrap());
            let page_no = u32::from_le_bytes(payload[4..8].try_into().unwrap());
            let slot = u16::from_le_bytes(payload[8..10].try_into().unwrap());
            let deleter = u32::from_le_bytes(payload[10..14].try_into().unwrap());
            Record::DeleteMark { file_no, rid: Rid::new(page_no, slot), deleter }
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
            (7u32, Record::Insert { file_no: 3, rid: rid(9, 4), record: vec![1, 2, 3] }),
            (8u32, Record::DeleteMark { file_no: 3, rid: rid(9, 4), deleter: 8 }),
            (9u32, Record::Commit),
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
        assert_eq!(plan.committed_ids, Vec::<u32>::new());
    }

    #[test]
    fn plan_groups_by_trx_in_log_order() {
        let mut bytes = Vec::new();
        bytes.extend(encode_frame(1, &Record::Insert { file_no: 0, rid: rid(1, 0), record: b"a".to_vec() }));
        bytes.extend(encode_frame(2, &Record::Insert { file_no: 0, rid: rid(1, 1), record: b"b".to_vec() }));
        bytes.extend(encode_frame(1, &Record::DeleteMark { file_no: 0, rid: rid(1, 1), deleter: 1 }));
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
    fn uncommitted_records_are_not_replayed() {
        let mut bytes = Vec::new();
        bytes.extend(encode_frame(5, &Record::Insert { file_no: 0, rid: rid(1, 0), record: b"x".to_vec() }));
        let plan = plan_recovery(&bytes);
        assert!(plan.committed.is_empty());
        assert!(plan.committed_ids.is_empty());
        assert_eq!(plan.max_trx_id, 5);
    }
}
