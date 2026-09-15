//! Checkpoint and vacuum methods of [`Database`].

use std::sync::atomic::Ordering;

use crate::error::{Error, Result};
use crate::index::{encode_key, BTree};

use super::Database;

impl Database {
    pub fn flush(&self) -> Result<()> {
        if !self.trx.no_open_transactions() {
            // truncating the log now would drop the open transaction's redo
            // records, so its later COMMIT could not be recovered
            return Err(Error::Runtime(
                "cannot flush while transactions are open".into(),
            ));
        }
        self.flush_inner(None)
    }

    /// Writes dirty buffer-pool pages and LSM memtables to their files without
    /// touching the WAL or the catalog. Used by the demo's disk inspector so a
    /// Refresh reflects writes still sitting in the pool. Safe while
    /// transactions are open (it only makes pages durable).
    pub(crate) fn flush_pages(&self) -> Result<()> {
        self.pool.flush_all()?;
        for meta in self.catalog().table_metas() {
            self.catalog().table(&meta.name)?.engine().flush()?;
        }
        Ok(())
    }

    /// Runs a checkpoint. `exclude` is the id of the statement's own
    /// (autocommit) transaction, which must not count as an open one.
    pub(crate) fn flush_inner(&self, exclude: Option<u64>) -> Result<()> {
        let _serial = self.checkpoint_lock.lock();
        // Writing pages back is safe while transactions are open; only
        // dropping the log needs the no-open guarantee below.
        let commits = self.trx.committed_count();
        self.pool.flush_all()?;
        // LSM tables flush their memtable to a durable SSTable; heap tables
        // are covered by the buffer-pool flush above.
        for meta in self.catalog().table_metas() {
            self.catalog().table(&meta.name)?.engine().flush()?;
        }
        self.save_catalog()?;
        // Truncate only if no transaction committed while we flushed (its
        // pages may not be in our snapshot) and none is open (its redo is
        // still needed). Holding the open set across the check and the
        // truncate keeps a begin or a commit from slipping between them.
        self.trx.with_open_set(|open| {
            let others_open = open.iter().any(|&id| Some(id) != exclude);
            if !others_open && self.trx.committed_count() == commits {
                self.wal.truncate()?;
            }
            Ok(())
        })?;
        // checkpoint is the natural reporting point for cache observability
        self.pool.report_stats();
        Ok(())
    }

    /// Physically removes rows no transaction can ever see again:
    /// delete-marked rows whose deleter committed, and orphan versions whose
    /// creator never committed (left behind by a crashed transaction).
    /// Stale index entries of purged rows are removed too. Must run with no
    /// open transactions (the VACUUM statement enforces this).
    pub(crate) fn vacuum(&self) -> Result<usize> {
        let mut purged = 0;
        // VACUUM runs with no open transaction, so the clog is final for every
        // xid: a version is dead if its creator never committed, or its deleter
        // did commit.
        let clog = self.trx.commit_status();
        let metas = self.catalog().table_metas();
        for meta in metas {
            let ops = self.index_ops(&meta.name)?;
            let engine = self.catalog().table(&meta.name)?.engine();
            for (rid, rec) in self.store_scan_raw(&meta.name)? {
                let (creator, deleter, row) = crate::storage::codec::decode_record(&rec, &self.lobs)?;
                let dead = !clog.is_committed(creator)
                    || (deleter != 0 && clog.is_committed(deleter));
                if dead {
                    for (ci, ix_file) in &ops {
                        let key = encode_key(&row[*ci])?;
                        BTree::at(*ix_file).delete(&self.pool, &key, rid)?;
                    }
                    self.free_lob_refs(&rec);
                    engine.delete(&self.pool, rid)?;
                    purged += 1;
                    continue;
                }
                // A live version whose delete marker was left by a transaction
                // that never committed is un-deleted, so the horizon below can
                // treat every old xid as committed.
                if deleter != 0 && !clog.is_committed(deleter) {
                    engine.delete_mark(&self.pool, rid, 0, 0)?;
                }
            }
        }
        // Every xid below the next unallocated one has ended (no transaction is
        // open) and no live version still references an aborted xid, so the
        // clog prefix is frozen and can be dropped, keeping xid state bounded.
        self.trx.advance_horizon(self.trx.next_id());
        self.save_catalog()?;
        Ok(purged)
    }

    /// Overrides the auto-checkpoint log budget in bytes; mainly for tests.
    pub fn set_wal_checkpoint_threshold(&self, bytes: u64) {
        self.wal_checkpoint_threshold.store(bytes, Ordering::Relaxed);
    }
}
