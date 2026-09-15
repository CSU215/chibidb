use crate::catalog::{ColumnDesc, Schema};
use crate::storage::codec::decode_record;
use crate::storage::page::FileId;
use crate::storage::Rid;
use crate::value::Value;
use crate::{Database, Result};

use super::{ExecContext, PhysicalOperator};
use crate::exec::chunk::{CHUNK_ROWS, Chunk};

/// The index-derived row ids a lowered access path selected, handed to the scan
/// as plain data so the operator never reaches back into the planner.
pub(crate) struct IndexScanPlan {
    pub(crate) column: String,
    /// Index name and a human-readable predicate, for EXPLAIN / visualisation.
    pub(crate) index: String,
    pub(crate) predicate: String,
    pub(crate) rids: Vec<Rid>,
}
/// Index scan: fetches exactly the row ids the access path selected.
pub struct IndexScan {
    table: String,
    schema: Schema,
    engine: std::sync::Arc<dyn crate::storage::engine::TableStorage>,
    column: String,
    /// Index name and predicate, kept for EXPLAIN / visualisation.
    index: String,
    predicate: String,
    /// True for the ORDER BY-driven in-order cursor (no WHERE selection).
    ordered: bool,
    rids: Vec<Rid>,
    /// The index file an in-order scan walks; the live cursor is built in
    /// `open`, so the same plan can be opened repeatedly (subquery re-runs).
    cursor_file: Option<FileId>,
    /// A lazy in-order cursor over the index; when set, `rids` is unused.
    cursor: Option<crate::index::LeafCursor>,
    pos: usize,
}

impl IndexScan {
    /// A range/equality scan over the row ids a lowered access path selected.
    /// `owner` is the alias (or table name) that qualifies this scan's columns.
    pub(crate) fn from_plan(
        db: &Database,
        table: &str,
        owner: &str,
        plan: IndexScanPlan,
    ) -> Result<Self> {
        let mut scan = Self::skeleton(db, table, owner, plan.column)?;
        scan.index = plan.index;
        scan.predicate = plan.predicate;
        scan.rids = plan.rids;
        Ok(scan)
    }

    /// Scans `column`'s index in ascending key order, so an `ORDER BY column`
    /// can reuse the index instead of sorting. The scan is lazy, so a LIMIT
    /// stops it once it has the rows it needs.
    pub(crate) fn from_cursor(
        db: &Database,
        table: &str,
        owner: &str,
        column: &str,
        file: FileId,
    ) -> Result<Self> {
        let mut scan = Self::skeleton(db, table, owner, column.to_string())?;
        scan.index = db
            .catalog()
            .indexes_for(table)
            .into_iter()
            .find(|ix| ix.column == column)
            .map(|ix| ix.name.clone())
            .unwrap_or_default();
        scan.ordered = true;
        scan.cursor_file = Some(file);
        Ok(scan)
    }

    fn skeleton(db: &Database, table: &str, owner: &str, column: String) -> Result<Self> {
        let (columns, engine) = {
            let catalog = db.catalog();
            let t = catalog.table(table)?;
            (t.schema.columns.clone(), t.engine())
        };
        let owner = owner.to_string();
        let schema = Schema {
            columns: columns
                .into_iter()
                .map(|c| ColumnDesc::plain(Some(owner.clone()), c.name, c.dtype))
                .collect(),
        };
        Ok(Self {
            table: table.to_string(),
            schema,
            engine,
            column,
            index: String::new(),
            predicate: String::new(),
            ordered: false,
            rids: Vec::new(),
            cursor_file: None,
            cursor: None,
            pos: 0,
        })
    }

    /// The indexed column, which the scan yields in ascending order.
    pub fn ordered_column(&self) -> &str {
        &self.column
    }

    /// Marks this scan as the ORDER BY access path, so EXPLAIN reports it as an
    /// `OrderedIndexScan` (the range/equality scan already yields key order).
    pub(crate) fn mark_ordered(&mut self) {
        self.ordered = true;
    }

    /// The next candidate rid, from the precomputed list or the lazy cursor.
    fn next_rid(&mut self, pool: &crate::storage::BufferPool) -> Result<Option<Rid>> {
        if let Some(cursor) = self.cursor.as_mut() {
            return cursor.next_rid(pool);
        }
        if self.pos < self.rids.len() {
            let rid = self.rids[self.pos];
            self.pos += 1;
            Ok(Some(rid))
        } else {
            Ok(None)
        }
    }
}

impl PhysicalOperator for IndexScan {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn label(&self) -> String {
        let kind = if self.ordered { "OrderedIndexScan" } else { "IndexScan" };
        if self.predicate.is_empty() {
            format!("{kind}(index={}, table={})", self.index, self.table)
        } else {
            format!(
                "{kind}(index={}, table={}, {})",
                self.index, self.table, self.predicate
            )
        }
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        ctx.db.note_read(ctx.trx.id, &self.table);
        self.pos = 0;
        self.cursor = match self.cursor_file {
            Some(file) => Some(crate::index::BTree::at(file).leaf_cursor(&ctx.db.pool)?),
            None => None,
        };
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        while let Some(rid) = self.next_rid(&ctx.db.pool)? {
            // a stale index entry may outlive its row (concurrent rollback)
            let Some(record) = self.engine.try_get(&ctx.db.pool, rid)? else {
                continue;
            };
            let (creator, deleter, row) = decode_record(&record, ctx.db.lobs())?;
            if ctx.trx.visible(creator, deleter) {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    fn next_chunk(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Chunk>> {
        let mut chunk = Chunk::with_schema(&self.schema);
        while chunk.len() < CHUNK_ROWS {
            let Some(rid) = self.next_rid(&ctx.db.pool)? else {
                break;
            };
            let Some(record) = self.engine.try_get(&ctx.db.pool, rid)? else {
                continue;
            };
            let (creator, deleter, row) = decode_record(&record, ctx.db.lobs())?;
            if ctx.trx.visible(creator, deleter) {
                chunk.push_row(&row)?;
            }
        }
        Ok((!chunk.is_empty()).then_some(chunk))
    }

    fn chunk_native(&self) -> bool {
        true
    }

    fn close(&mut self) -> Result<()> {
        self.pos = self.rids.len();
        self.cursor = None;
        Ok(())
    }
}