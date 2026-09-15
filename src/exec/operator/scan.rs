use crate::catalog::{ColumnDesc, Schema};
use crate::storage::codec::decode_record;
use crate::storage::engine::RowScanner;
use crate::value::{DataType, Value};
use crate::{Database, Result};

use super::{ExecContext, PhysicalOperator, ProjectedSink};
use crate::exec::chunk::{CHUNK_ROWS, Chunk};
/// Sequential scan of a table, filtering by MVCC visibility.
pub struct TableScan {
    table: String,
    schema: Schema,
    scanner: Option<Box<dyn RowScanner>>,
    /// Per base-column flag: `false` means the query never reads that column,
    /// so a large object stored there need not be resolved. `None` reads all.
    keep: Option<Vec<bool>>,
    /// Reused buffer holding the current encoded record, so scanning does not
    /// allocate per row.
    record: Vec<u8>,
    /// Reused row buffer for the fused aggregate path.
    row_buf: Vec<Value>,
}

impl TableScan {
    pub fn new(db: &Database, table: &str) -> Result<Self> {
        Self::with_owner(db, table, table)
    }

    /// `owner` is the alias (or table name) that qualifies this scan's
    /// columns, so qualified references resolve correctly.
    pub fn with_owner(db: &Database, table: &str, owner: &str) -> Result<Self> {
        Self::with_owner_keep(db, table, owner, None)
    }

    pub fn with_owner_keep(
        db: &Database,
        table: &str,
        owner: &str,
        keep: Option<Vec<bool>>,
    ) -> Result<Self> {
        let columns = db.catalog().table(table)?.schema.columns.clone();
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
            scanner: None,
            keep,
            record: Vec::new(),
            row_buf: Vec::new(),
        })
    }
}

impl PhysicalOperator for TableScan {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        // serializable conflict tracking: this transaction reads the table
        ctx.db.note_read(ctx.trx.id, &self.table);
        let engine = ctx.db.catalog().table(&self.table)?.engine();
        self.scanner = Some(match &self.keep {
            // A columnar table can skip the columns the query never reads.
            Some(keep) => engine.scan_projected(&ctx.db.pool, keep)?,
            None => engine.scan(&ctx.db.pool)?,
        });
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        loop {
            let found = {
                let scanner = self.scanner.as_mut().expect("table scan not opened");
                scanner.next_into(&ctx.db.pool, &mut self.record)?
            };
            if found.is_none() {
                return Ok(None);
            }
            let (creator, deleter, row) = match &self.keep {
                Some(keep) => {
                    crate::storage::codec::decode_record_pruned(&self.record, ctx.db.lobs(), keep)?
                }
                None => decode_record(&self.record, ctx.db.lobs())?,
            };
            if ctx.trx.visible(creator, deleter) {
                return Ok(Some(row));
            }
        }
    }

    fn next_chunk(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Chunk>> {
        let mut chunk = Chunk::with_schema(&self.schema);
        while chunk.len() < CHUNK_ROWS {
            let found = {
                let scanner = self.scanner.as_mut().expect("table scan not opened");
                scanner.next_into(&ctx.db.pool, &mut self.record)?
            };
            if found.is_none() {
                break;
            }
            let (creator, deleter) = crate::storage::codec::record_version(&self.record)?;
            if !ctx.trx.visible(creator, deleter) {
                continue;
            }
            chunk.push_encoded_row(
                &self.record[crate::storage::codec::RECORD_HEADER..],
                ctx.db.lobs(),
                self.keep.as_deref(),
            )?;
        }
        Ok((!chunk.is_empty()).then_some(chunk))
    }

    fn chunk_native(&self) -> bool {
        true
    }

    fn for_each_row(
        &mut self,
        ctx: &mut ExecContext<'_>,
        sink: &mut dyn FnMut(&[Value]) -> Result<()>,
    ) -> Result<bool> {
        loop {
            let found = {
                let scanner = self.scanner.as_mut().expect("table scan not opened");
                scanner.next_into(&ctx.db.pool, &mut self.record)?
            };
            if found.is_none() {
                break;
            }
            let (creator, deleter) = crate::storage::codec::record_version(&self.record)?;
            if !ctx.trx.visible(creator, deleter) {
                continue;
            }
            crate::storage::codec::decode_row_into(
                &self.record[crate::storage::codec::RECORD_HEADER..],
                Some(ctx.db.lobs()),
                self.keep.as_deref(),
                &mut self.row_buf,
            )?;
            sink(&self.row_buf)?;
        }
        Ok(true)
    }

    fn for_each_projected_row(
        &mut self,
        ctx: &mut ExecContext<'_>,
        cols: &[usize],
        sink: &mut ProjectedSink<'_>,
    ) -> Result<Option<bool>> {
        ctx.db.note_read(ctx.trx.id, &self.table);
        let engine = ctx.db.catalog().table(&self.table)?.engine();
        // The engine scans pages in place; filter to visible versions here.
        let mut sink = VisibleSink { trx: &*ctx.trx, sink };
        if engine.for_each_projected(&ctx.db.pool, cols, ctx.db.lobs(), &mut sink)? {
            return Ok(Some(true));
        }
        Ok(None)
    }

    fn close(&mut self) -> Result<()> {
        self.scanner = None;
        Ok(())
    }

    fn label(&self) -> String {
        format!("FullScan table={}", self.table)
    }
}

/// Forwards projected rows to a downstream sink, dropping versions the active
/// transaction cannot see.
struct VisibleSink<'a> {
    trx: &'a crate::txn::trx::TrxState,
    sink: &'a mut ProjectedSink<'a>,
}

impl crate::storage::engine::RowSink for VisibleSink<'_> {
    fn row(&mut self, creator: u64, deleter: u64, values: &[Value]) -> Result<()> {
        if self.trx.visible(creator, deleter) {
            (self.sink)(creator, deleter, values)?;
        }
        Ok(())
    }
}

/// Scans a view by materializing its already-lowered sub-plan. The view's
/// columns are exposed with `owner` (the alias or view name) and `Text`
/// placeholders, matching the materialized view path.
pub struct ViewScan {
    child: Box<dyn PhysicalOperator>,
    schema: Schema,
    rows: Vec<Vec<Value>>,
    pos: usize,
}

impl ViewScan {
    /// `owner` is the alias (or view name) that qualifies the view's columns.
    pub(crate) fn new(child: Box<dyn PhysicalOperator>, owner: &str) -> Self {
        let schema = Schema {
            columns: child
                .schema()
                .columns
                .iter()
                .map(|c| ColumnDesc::plain(Some(owner.to_string()), c.name.clone(), DataType::Text))
                .collect(),
        };
        Self { child, schema, rows: Vec::new(), pos: 0 }
    }
}

impl PhysicalOperator for ViewScan {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn label(&self) -> String {
        "ViewScan".to_string()
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.child.open(ctx)?;
        self.rows.clear();
        while let Some(row) = self.child.next(ctx)? {
            self.rows.push(row);
        }
        self.child.close()?;
        self.pos = 0;
        Ok(())
    }

    fn next(&mut self, _ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        if self.pos >= self.rows.len() {
            return Ok(None);
        }
        let row = self.rows[self.pos].clone();
        self.pos += 1;
        Ok(Some(row))
    }

    fn close(&mut self) -> Result<()> {
        self.rows.clear();
        self.pos = 0;
        Ok(())
    }
}

/// Produces exactly one empty tuple, for SELECTs without a FROM clause.
#[derive(Default)]
pub struct ConstantScan {
    schema: Schema,
    done: bool,
}

impl ConstantScan {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PhysicalOperator for ConstantScan {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn label(&self) -> String {
        "ConstantScan".to_string()
    }

    fn open(&mut self, _ctx: &mut ExecContext<'_>) -> Result<()> {
        self.done = false;
        Ok(())
    }

    fn next(&mut self, _ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(Some(Vec::new()))
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }
}
