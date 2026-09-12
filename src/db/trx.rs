use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::storage::Rid;

/// Source of process-unique session ids, used to attribute the 2PL write lock.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Sentinel transaction id for an autocommitted read-only snapshot. Real
/// transactions are numbered from 1, so this can never collide with a creator
/// or deleter id.
pub(crate) const READ_ONLY_TRX_ID: u32 = 0;

/// Per-connection state: at most one active transaction at a time, plus the
/// database the statements route to (selected with `USE`).
pub struct Session {
    pub(crate) trx: Option<TrxState>,
    current_db: Option<String>,
    /// Process-unique id, used to attribute the 2PL database write lock.
    id: u64,
    /// Whether this session currently holds the database's 2PL write lock.
    holds_writer: bool,
    /// Authenticated user, once the session has logged in.
    user: Option<String>,
}

impl Session {
    pub fn new() -> Self {
        Self {
            trx: None,
            current_db: None,
            id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            holds_writer: false,
            user: None,
        }
    }

    pub fn current_db(&self) -> Option<&str> {
        self.current_db.as_deref()
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

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn holds_writer(&self) -> bool {
        self.holds_writer
    }

    pub(crate) fn set_holds_writer(&mut self, held: bool) {
        self.holds_writer = held;
    }

    pub(crate) fn begin(&mut self, id: u32, committed: &HashSet<u32>, explicit: bool) {
        self.trx = Some(TrxState {
            id,
            snapshot: committed.clone(),
            undo: Vec::new(),
            explicit,
        });
    }

    /// Begins the snapshot used by an autocommitted read-only statement. It
    /// carries a snapshot but no undo log, never commits and is never
    /// registered in the database's bookkeeping, so readers do not mutate
    /// shared state. The id is `READ_ONLY_TRX_ID`, which no real transaction
    /// uses (ids start at 1).
    pub(crate) fn begin_readonly(&mut self, committed: &HashSet<u32>) {
        self.trx = Some(TrxState {
            id: READ_ONLY_TRX_ID,
            snapshot: committed.clone(),
            undo: Vec::new(),
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
    /// Own delete-mark: clear the marker on rollback. `prev_deleter` is the
    /// marker before our write, used by first-committer-wins conflict checks.
    DeleteMark { table: String, rid: Rid, prev_deleter: u32 },
    /// MVCC update: remove the new version, unmark the old one.
    Update {
        table: String,
        old_rid: Rid,
        new_rid: Rid,
        new_row: Vec<crate::value::Value>,
        prev_deleter: u32,
    },
}
