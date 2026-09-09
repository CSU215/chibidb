use std::collections::HashSet;

use crate::storage::Rid;

/// Per-connection state: at most one active transaction at a time.
pub struct Session {
    pub(crate) trx: Option<TrxState>,
}

impl Session {
    pub fn new() -> Self {
        Self { trx: None }
    }

    pub(crate) fn begin(&mut self, id: u32, committed: &HashSet<u32>, explicit: bool) {
        self.trx = Some(TrxState {
            id,
            snapshot: committed.clone(),
            undo: Vec::new(),
            explicit,
        });
    }

    pub(crate) fn trx(&mut self) -> &mut TrxState {
        self.trx.as_mut().expect("no active transaction")
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) struct TrxState {
    pub id: u32,
    /// Transactions that were committed when this transaction began.
    pub snapshot: HashSet<u32>,
    pub undo: Vec<Undo>,
    /// True for BEGIN-initiated transactions (DDL is rejected inside those).
    pub explicit: bool,
}

impl TrxState {
    /// Snapshot-isolation visibility of a row version.
    pub fn visible(&self, creator: u32, deleter: u32) -> bool {
        let creator_visible =
            creator == 0 || creator == self.id || self.snapshot.contains(&creator);
        // a row is gone for me if I deleted it myself, or the deleter
        // committed before my snapshot
        let deleted_for_me =
            deleter != 0 && (deleter == self.id || self.snapshot.contains(&deleter));
        creator_visible && !deleted_for_me
    }
}

#[derive(Debug)]
pub(crate) enum Undo {
    /// Own insert: physically remove on rollback.
    Insert { table: String, rid: Rid, row: Vec<crate::value::Value> },
    /// Own delete-mark: clear the marker on rollback.
    DeleteMark { table: String, rid: Rid },
    /// MVCC update: remove the new version, unmark the old one.
    Update { table: String, old_rid: Rid, new_rid: Rid, new_row: Vec<crate::value::Value> },
}
