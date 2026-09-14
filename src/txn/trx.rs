#![allow(dead_code)]

use std::sync::Arc;

use crate::txn::clog::CommitStatus;
use crate::txn::transaction::Snapshot;
use crate::storage::Rid;

/// Sentinel transaction id for an autocommitted read-only snapshot. Real
/// transactions are numbered from 1, so this can never collide with a creator
/// or deleter id.
pub(crate) const READ_ONLY_TRX_ID: u64 = 0;

/// Per-connection state: at most one active transaction at a time, plus the
/// database the statements route to (selected with `USE`).
pub struct Session {
    pub(crate) trx: Option<TrxState>,
    current_db: Option<String>,
    /// Authenticated user, once the session has logged in.
    user: Option<String>,
    /// Whether each statement commits on its own (the MySQL frontend tracks
    /// the client's `SET autocommit`; the engine always starts sessions in
    /// autocommit mode).
    autocommit: bool,
}

impl Session {
    pub fn new() -> Self {
        Self { trx: None, current_db: None, user: None, autocommit: true }
    }

    pub fn current_db(&self) -> Option<&str> {
        self.current_db.as_deref()
    }

    /// Whether the session is in autocommit mode.
    pub fn autocommit(&self) -> bool {
        self.autocommit
    }

    pub(crate) fn set_autocommit(&mut self, autocommit: bool) {
        self.autocommit = autocommit;
    }

    /// Whether a transaction (explicit or otherwise) is currently open.
    pub fn in_transaction(&self) -> bool {
        self.trx.is_some()
    }

    /// The authenticated user, if the session has logged in.
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    pub(crate) fn set_user(&mut self, user: Option<String>) {
        self.user = user;
    }

    pub(crate) fn set_current_db(&mut self, name: Option<String>) {
        self.current_db = name;
    }

    pub(crate) fn begin(
        &mut self,
        id: u64,
        snapshot: Snapshot,
        clog: Arc<CommitStatus>,
        explicit: bool,
    ) {
        self.trx = Some(TrxState {
            id,
            snapshot,
            clog,
            undo: Vec::new(),
            wal: Vec::new(),
            explicit,
        });
    }

    /// Begins the snapshot used by an autocommitted read-only statement. It
    /// carries a snapshot but no undo log, never commits and is never
    /// registered in the database's bookkeeping, so readers do not mutate
    /// shared state. The id is `READ_ONLY_TRX_ID`, which no real transaction
    /// uses (ids start at 1).
    pub(crate) fn begin_readonly(&mut self, snapshot: Snapshot, clog: Arc<CommitStatus>) {
        self.trx = Some(TrxState {
            id: READ_ONLY_TRX_ID,
            snapshot,
            clog,
            undo: Vec::new(),
            wal: Vec::new(),
            explicit: false,
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
    pub id: u64,
    /// PostgreSQL-style snapshot: an xid is visible if the clog says it
    /// committed and it was neither in flight nor allocated after this
    /// snapshot.
    pub snapshot: Snapshot,
    /// The shared commit-status bitmap this snapshot consults.
    pub clog: Arc<CommitStatus>,
    pub undo: Vec<Undo>,
    /// Redo frames for this transaction's writes, buffered until commit so the
    /// whole statement reaches the log in one write. Rolled back by truncating.
    pub wal: Vec<u8>,
    /// True for BEGIN-initiated transactions (DDL is rejected inside those).
    pub explicit: bool,
}

impl TrxState {
    /// Snapshot-isolation visibility of a row version.
    pub fn visible(&self, creator: u64, deleter: u64) -> bool {
        let creator_visible =
            creator == 0 || creator == self.id || self.committed_before(creator);
        // a row is gone for me if I deleted it myself, or the deleter
        // committed before my snapshot
        let deleted_for_me =
            deleter != 0 && (deleter == self.id || self.committed_before(deleter));
        creator_visible && !deleted_for_me
    }

    /// Whether `xid` had already committed when this transaction's snapshot was
    /// taken. An xid that was in flight then, or was allocated after, does not
    /// count even if it has since committed.
    pub(crate) fn committed_before(&self, xid: u64) -> bool {
        xid < self.snapshot.xmax
            && self.snapshot.xip.binary_search(&xid).is_err()
            && self.clog.is_committed(xid)
    }
}

#[derive(Debug)]
pub(crate) enum Undo {
    /// Own insert: physically remove on rollback.
    Insert { table: String, rid: Rid, row: Vec<crate::value::Value> },
    /// Own delete-mark: clear the marker on rollback. `prev_deleter` is the
    /// marker before our write, used by first-committer-wins conflict checks.
    DeleteMark { table: String, rid: Rid, prev_deleter: u64, prev_next_rid: u64 },
    /// MVCC update: remove the new version, unmark the old one.
    Update {
        table: String,
        old_rid: Rid,
        new_rid: Rid,
        new_row: Vec<crate::value::Value>,
        prev_deleter: u64,
        prev_next_rid: u64,
    },
}
