//! Unique-constraint enforcement methods of [`Database`].

use crate::error::{Error, Result};
use crate::index::{encode_key, BTree};
use crate::storage::codec::{decode_record, record_next_rid, unpack_rid};
use crate::storage::engine::TableStorage;
use crate::storage::{FileId, Rid};
use crate::txn::trx::TrxState;
use crate::value::Value;

use super::Database;

impl Database {
    /// Enforces UNIQUE / PRIMARY KEY constraints for `row` using the
    /// constraint-backed indexes. `exclude` skips the row being updated;
    /// `claimed` catches duplicates among rows touched by the same statement
    /// before they reach the index. Claims are keyed by column index so equal
    /// values in different constraint columns do not collide.
    ///
    /// Uniqueness is a property of the index, not of a snapshot: the check
    /// takes a per-key lock (held to transaction end, like a row lock) and then
    /// asks whether *any committed* version still holds the key. The loser of a
    /// concurrent race therefore waits for the winner to finish and then sees
    /// its committed row, even if the winner committed after the loser's
    /// snapshot.
    pub(crate) fn check_unique(
        &self,
        table: &str,
        row: &[Value],
        exclude: Option<Rid>,
        trx: &TrxState,
        claimed: &mut Vec<(usize, Vec<u8>)>,
    ) -> Result<()> {
        let (checks, engine) = {
            let catalog = self.catalog();
            let schema = &catalog.table(table)?.schema;
            let checks: Vec<(usize, FileId, String)> = catalog
                .unique_indexes_for(table)
                .into_iter()
                .map(|ix| {
                    let ci = schema.index_of(&ix.column).expect("index column validated");
                    (ci, ix.store.file, ix.column.clone())
                })
                .collect();
            (checks, catalog.table(table)?.engine())
        };
        if checks.is_empty() {
            return Ok(());
        }
        // the uniqueness check reads the index: a concurrent writer of the same
        // table is an rw-antidependency under serializable.
        self.note_read(trx.id, table);
        for (ci, ix_file, column) in checks {
            if matches!(row[ci], Value::Null) {
                continue; // UNIQUE permits multiple NULLs
            }
            let key = encode_key(&row[ci])?;
            if claimed.iter().any(|(c, k)| *c == ci && k == &key) {
                return Err(Error::Runtime(format!("duplicate key: {table}({column})")));
            }
            claimed.push((ci, key.clone()));
            // serialize with any other transaction checking/inserting this key
            self.lock_unique_key(trx.id, table, ix_file, &key)?;
            for rid in BTree::at(ix_file).search(&self.pool, &key)? {
                if Some(rid) == exclude {
                    continue;
                }
                let Some(rec) = engine.try_get(&self.pool, rid)? else {
                    continue; // stale index entry (row removed by a rollback)
                };
                let (creator, deleter, _) = decode_record(&rec, &self.lobs)?;
                if !self.unique_key_taken(creator, deleter, trx.id) {
                    continue;
                }
                // A version of the row being updated shares the key but is not
                // a duplicate; the update conflict is resolved by EPQ. The
                // chain link may have been created while we waited on the key
                // lock, so this must be decided now, not up front.
                if let Some(ex) = exclude
                    && self.same_version_chain(&*engine, ex, rid)?
                {
                    continue;
                }
                return Err(Error::Runtime(format!("duplicate key: {table}({column})")));
            }
        }
        Ok(())
    }

    /// Whether two rids are versions of the same logical row, i.e. connected by
    /// the `next_rid` update chain.
    fn same_version_chain(&self, engine: &dyn TableStorage, a: Rid, b: Rid) -> Result<bool> {
        Ok(a == b || self.chain_reaches(engine, a, b)? || self.chain_reaches(engine, b, a)?)
    }

    /// Whether the chain starting at `from` reaches `target`.
    fn chain_reaches(&self, engine: &dyn TableStorage, from: Rid, target: Rid) -> Result<bool> {
        let mut cur = from;
        for _ in 0..4096 {
            let Some(rec) = engine.try_get(&self.pool, cur)? else {
                return Ok(false);
            };
            let next = record_next_rid(&rec)?;
            if next == 0 {
                return Ok(false);
            }
            let (page, slot) = unpack_rid(next);
            let next = Rid::new(page, slot);
            if next == target {
                return Ok(true);
            }
            if next == cur {
                return Ok(false);
            }
            cur = next;
        }
        Ok(false)
    }

    /// Whether a version still occupies its unique key for the current
    /// (committed) state: its creator is committed — or is this transaction —
    /// and it has not been deleted by this transaction or a committed one.
    fn unique_key_taken(&self, creator: u64, deleter: u64, self_id: u64) -> bool {
        let creator_live = creator == self_id || self.trx.is_committed(creator);
        let deleted = deleter != 0 && (deleter == self_id || self.trx.is_committed(deleter));
        creator_live && !deleted
    }

    /// Takes the index-key lock that serializes concurrent uniqueness checks of
    /// the same key. It shares the row-lock manager, so it is released with the
    /// transaction's other locks; the leading control byte keeps the synthetic
    /// name from colliding with a real table's row-lock keys.
    fn lock_unique_key(&self, owner: u64, table: &str, ix_file: FileId, key: &[u8]) -> Result<()> {
        use std::fmt::Write as _;
        let mut name = String::with_capacity(table.len() + 2 * key.len() + 12);
        let _ = write!(name, "\u{1}{table}\u{1}{ix_file}\u{1}");
        for b in key {
            let _ = write!(name, "{b:02x}");
        }
        self.locks.lock(owner, &name, Rid::new(0, 0))
    }
}
