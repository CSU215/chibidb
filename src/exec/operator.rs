use std::collections::{HashMap, HashSet, VecDeque};

use crate::ast::{
    BinOp, DataType, Expr, JoinKind, Limit as LimitClause, SelectItem, SelectStmt, Stmt, TableRef,
};
use crate::catalog::{ColumnDesc, Schema};
use crate::config::ExecutionMode;
use crate::index::encode_key_into;
use crate::storage::codec::decode_record;
use crate::storage::engine::RowScanner;
use crate::storage::Rid;
use crate::value::Value;
use crate::{Database, Error, Result};

use super::aggregate::{expr_has_aggregate, sort_rows};
use super::chunk::{CHUNK_ROWS, Chunk, Column};
use super::eval::{eval_binary, eval_const, EvalCtx};
use super::subquery::{eval_bound, eval_predicate_bound};

/// Build-row indices matching one key. The common unique-key case stays inline
/// so building the table does not allocate a `Vec` per key.
enum MatchList {
    One(usize),
    Many(Vec<usize>),
}

impl MatchList {
    fn push(&mut self, row: usize) {
        match self {
            MatchList::One(first) => *self = MatchList::Many(vec![*first, row]),
            MatchList::Many(rows) => rows.push(row),
        }
    }

    fn copy_into(&self, out: &mut Vec<usize>) {
        match self {
            MatchList::One(row) => out.push(*row),
            MatchList::Many(rows) => out.extend_from_slice(rows),
        }
    }
}

/// Context threaded through operators: the database, the session's active
/// transaction, and the outer row/group context when this plan runs as a
/// correlated subquery.
pub struct ExecContext<'a> {
    pub(crate) db: &'a Database,
    pub(crate) trx: &'a mut crate::trx::TrxState,
    pub(crate) outer: Option<&'a EvalCtx<'a>>,
}

/// Receives `(creator, deleter, projected values)` for one row.
pub(crate) type ProjectedSink<'a> = dyn FnMut(u64, u64, &[Value]) -> Result<()> + 'a;

/// Whether a plan streams rows or is a side-effecting command (DML).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    Rows,
    Command,
}

/// Volcano-style physical operator: `open`, repeated `next`, `close`.
pub trait PhysicalOperator {
    fn schema(&self) -> &Schema;
    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()>;
    fn next(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>>;

    /// Emits one columnar batch, or `None` at EOF.
    ///
    /// The default bridges the row interface, so every operator already works
    /// in chunk mode; operators with a native columnar path override this and
    /// [`PhysicalOperator::chunk_native`].
    fn next_chunk(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Chunk>> {
        match self.next(ctx)? {
            Some(row) => Ok(Some(Chunk::from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// Whether [`PhysicalOperator::next_chunk`] is a native columnar path.
    fn chunk_native(&self) -> bool {
        false
    }

    /// Streams decoded rows to `sink` without materializing chunks, when this
    /// operator can read storage directly. Returns `Ok(false)` if there is no
    /// fused path, so the caller falls back to [`PhysicalOperator::next_chunk`].
    fn for_each_row(
        &mut self,
        _ctx: &mut ExecContext<'_>,
        _sink: &mut dyn FnMut(&[Value]) -> Result<()>,
    ) -> Result<bool> {
        Ok(false)
    }

    /// Streams one row's requested base columns (`values[i]` is column
    /// `cols[i]`; empty `cols` streams versions only) when this operator is a
    /// bare scan, so aggregates need not rebuild rows. `Ok(None)` means no
    /// columnar path; the caller falls back to
    /// [`PhysicalOperator::for_each_row`].
    fn for_each_projected_row(
        &mut self,
        _ctx: &mut ExecContext<'_>,
        _cols: &[usize],
        _sink: &mut ProjectedSink<'_>,
    ) -> Result<Option<bool>> {
        Ok(None)
    }

    fn close(&mut self) -> Result<()>;

    /// Commands (DML) perform their work in `open` and yield no rows.
    fn output_kind(&self) -> OutputKind {
        OutputKind::Rows
    }

    /// How many rows a command changed, once `open` has run. `None` for plans
    /// that stream rows or do not track a count.
    fn affected_rows(&self) -> Option<u64> {
        None
    }

    /// This operator's name, as the plan panel shows it.
    ///
    /// Read-only and side-effect free: describing a tree must not open, read or
    /// otherwise disturb anything, so that rendering a plan cannot change the
    /// thing it describes.
    fn name(&self) -> &'static str {
        "Operator"
    }

    /// Description lines for the plan panel, in display order. Only what the
    /// operator already knows -- nothing is looked up or recomputed here.
    fn details(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }

    /// The operators feeding this one, in execution order. Empty for scans.
    ///
    /// This is what makes the rendered tree the tree that runs: the plan panel
    /// walks these, so a new operator shows up in the panel by implementing
    /// this method rather than by editing a description elsewhere.
    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        Vec::new()
    }
}

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
    fn name(&self) -> &'static str {
        "TableScan"
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        let mut out = vec![("table", self.table.clone())];
        // Column pruning skips large objects the query never reads; saying so
        // explains why this scan is cheaper than the row count suggests.
        if let Some(keep) = &self.keep {
            let read = keep.iter().filter(|k| **k).count();
            out.push(("columns", format!("{read} of {}", keep.len())));
        }
        out
    }

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
}

/// Forwards projected rows to a downstream sink, dropping versions the active
/// transaction cannot see.
struct VisibleSink<'a> {
    trx: &'a crate::trx::TrxState,
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

/// Scans a view by running its stored SELECT as a sub-plan. The view's columns
/// are exposed with `owner` (the alias or view name) and `Text` placeholders,
/// matching the materialized view path.
pub struct ViewScan {
    child: Box<dyn PhysicalOperator>,
    schema: Schema,
    rows: Vec<Vec<Value>>,
    pos: usize,
}

impl ViewScan {
    pub fn new(db: &Database, view_sql: &str, owner: &str) -> Result<Option<Self>> {
        let stmts = crate::parser::parse(view_sql)?;
        let Some(crate::ast::Stmt::Select(select)) = stmts.into_iter().next() else {
            return Ok(None);
        };
        let Some(plan) = build_select(db, &select)? else {
            return Ok(None);
        };
        let schema = Schema {
            columns: plan
                .schema()
                .columns
                .iter()
                .map(|c| ColumnDesc::plain(Some(owner.to_string()), c.name.clone(), DataType::Text))
                .collect(),
        };
        Ok(Some(Self { child: plan, schema, rows: Vec::new(), pos: 0 }))
    }
}

impl PhysicalOperator for ViewScan {
    fn name(&self) -> &'static str {
        "ViewScan"
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        match self.schema.columns.first().and_then(|c| c.owner.clone()) {
            Some(view) => vec![("view", view)],
            None => Vec::new(),
        }
    }

    /// The view's own plan: a view is not a scan of stored rows, it is a plan
    /// that runs in place, and the panel has to show that rather than hide it.
    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
    }

    fn schema(&self) -> &Schema {
        &self.schema
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
    fn name(&self) -> &'static str {
        "ConstantScan"
    }

    fn schema(&self) -> &Schema {
        &self.schema
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

/// Resolves a bare column reference against `schema` for the zero-copy
/// projection path; `None` means fall back to row-wise evaluation.
fn simple_column(schema: &Schema, expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Column(name) => schema.resolve(None, name).ok(),
        Expr::QualifiedColumn(owner, name) => schema.resolve(Some(owner), name).ok(),
        _ => None,
    }
}

fn is_comparison(op: BinOp) -> bool {
    matches!(op, BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
}

/// One side of a vectorized comparison: a column of the current chunk, or a
/// constant evaluated once.
enum Operand<'a> {
    Column(&'a Column),
    Const(Value),
}

impl Operand<'_> {
    fn value(&self, index: usize) -> Value {
        match self {
            Operand::Column(column) => column.value(index),
            Operand::Const(value) => value.clone(),
        }
    }
}

fn operand<'a>(expr: &Expr, schema: &Schema, chunk: &'a Chunk) -> Option<Operand<'a>> {
    if let Some(index) = simple_column(schema, expr) {
        return Some(Operand::Column(chunk.column(index)));
    }
    eval_const(expr).ok().map(Operand::Const)
}

/// Column-wise WHERE evaluation for comparisons, `and`/`or` and `is [not]
/// null`, reusing [`eval_binary`] so NULL and coercion semantics match the row
/// path. Returns `None` for shapes that need rows (subqueries, computed
/// operands, `not`, `in`, ...), so the caller falls back.
fn predicate_mask(expr: &Expr, chunk: &Chunk, schema: &Schema) -> Result<Option<Vec<bool>>> {
    match expr {
        Expr::Binary(BinOp::And, l, r) => {
            match (predicate_mask(l, chunk, schema)?, predicate_mask(r, chunk, schema)?) {
                (Some(a), Some(b)) => Ok(Some(a.iter().zip(&b).map(|(x, y)| *x && *y).collect())),
                _ => Ok(None),
            }
        }
        Expr::Binary(BinOp::Or, l, r) => {
            match (predicate_mask(l, chunk, schema)?, predicate_mask(r, chunk, schema)?) {
                (Some(a), Some(b)) => Ok(Some(a.iter().zip(&b).map(|(x, y)| *x || *y).collect())),
                _ => Ok(None),
            }
        }
        Expr::Binary(op, l, r) if is_comparison(*op) => {
            let (Some(left), Some(right)) =
                (operand(l, schema, chunk), operand(r, schema, chunk))
            else {
                return Ok(None);
            };
            let mut mask = Vec::with_capacity(chunk.len());
            for i in 0..chunk.len() {
                let passed = eval_binary(*op, left.value(i), right.value(i))?;
                mask.push(matches!(passed, Value::Bool(true)));
            }
            Ok(Some(mask))
        }
        Expr::IsNull(inner, negated) => match simple_column(schema, inner) {
            Some(index) => {
                let column = chunk.column(index);
                Ok(Some((0..chunk.len()).map(|i| column.is_null(i) != *negated).collect()))
            }
            None => Ok(None),
        },
        _ => Ok(None),
    }
}

/// Drops rows whose predicate does not evaluate to true.
pub struct Filter {
    child: Box<dyn PhysicalOperator>,
    predicate: Expr,
}

impl Filter {
    pub fn new(child: Box<dyn PhysicalOperator>, predicate: Expr) -> Self {
        Self { child, predicate }
    }
}

impl PhysicalOperator for Filter {
    fn name(&self) -> &'static str {
        "Filter"
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        vec![("predicate", self.predicate.to_string())]
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
    }

    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.child.open(ctx)
    }

    fn next(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        loop {
            let Some(row) = self.child.next(ctx)? else {
                return Ok(None);
            };
            if eval_predicate_bound(
                ctx.db,
                ctx.trx,
                &self.predicate,
                self.child.schema(),
                &row,
                ctx.outer,
            )? {
                return Ok(Some(row));
            }
        }
    }

    fn next_chunk(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Chunk>> {
        loop {
            let Some(chunk) = self.child.next_chunk(ctx)? else {
                return Ok(None);
            };
            let schema = self.child.schema();
            let keep: Vec<usize> = match predicate_mask(&self.predicate, &chunk, schema)? {
                Some(mask) => {
                    mask.iter().enumerate().filter_map(|(i, &pass)| pass.then_some(i)).collect()
                }
                None => {
                    let mut keep = Vec::new();
                    let mut row = Vec::with_capacity(chunk.num_columns());
                    for i in 0..chunk.len() {
                        chunk.row_into(i, &mut row);
                        if eval_predicate_bound(
                            ctx.db,
                            ctx.trx,
                            &self.predicate,
                            schema,
                            &row,
                            ctx.outer,
                        )? {
                            keep.push(i);
                        }
                    }
                    keep
                }
            };
            if !keep.is_empty() {
                return Ok(Some(chunk.take(&keep)));
            }
        }
    }

    fn chunk_native(&self) -> bool {
        true
    }

    fn close(&mut self) -> Result<()> {
        self.child.close()
    }
}

/// Evaluates projection expressions over each child row.
pub struct Project {
    child: Box<dyn PhysicalOperator>,
    exprs: Vec<Expr>,
    schema: Schema,
}

impl Project {
    pub fn new(child: Box<dyn PhysicalOperator>, exprs: Vec<Expr>, headers: Vec<String>) -> Self {
        let schema = Schema {
            columns: headers
                .into_iter()
                .map(|h| ColumnDesc::plain(None, h, DataType::Text))
                .collect(),
        };
        Self { child, exprs, schema }
    }
}

impl PhysicalOperator for Project {
    fn name(&self) -> &'static str {
        "Project"
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        let columns: Vec<String> = self.schema.columns.iter().map(|c| c.name.clone()).collect();
        vec![("columns", columns.join(", "))]
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.child.open(ctx)
    }

    fn next(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        let Some(row) = self.child.next(ctx)? else {
            return Ok(None);
        };
        let mut eval_ctx = EvalCtx::row(self.child.schema(), &row);
        eval_ctx.parent = ctx.outer;
        let mut out = Vec::with_capacity(self.exprs.len());
        for expr in &self.exprs {
            out.push(eval_bound(ctx.db, ctx.trx, expr, Some(&eval_ctx))?);
        }
        Ok(Some(out))
    }

    fn next_chunk(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Chunk>> {
        let Some(chunk) = self.child.next_chunk(ctx)? else {
            return Ok(None);
        };
        let schema = self.child.schema();
        let mut columns = Vec::with_capacity(self.exprs.len());
        let mut row = Vec::with_capacity(chunk.num_columns());
        for expr in &self.exprs {
            // Bare column: reuse the source column without touching rows.
            if let Some(index) = simple_column(schema, expr) {
                columns.push(chunk.column(index).clone());
                continue;
            }
            let mut values = Vec::with_capacity(chunk.len());
            for i in 0..chunk.len() {
                chunk.row_into(i, &mut row);
                let mut eval_ctx = EvalCtx::row(schema, &row);
                eval_ctx.parent = ctx.outer;
                values.push(eval_bound(ctx.db, ctx.trx, expr, Some(&eval_ctx))?);
            }
            columns.push(Column::from_values(&values)?);
        }
        Ok(Some(Chunk::from_columns(columns)))
    }

    fn chunk_native(&self) -> bool {
        true
    }

    fn close(&mut self) -> Result<()> {
        self.child.close()
    }
}

/// Skips `offset` rows, then emits up to `count` rows (`None` = unlimited).
pub struct Limit {
    child: Box<dyn PhysicalOperator>,
    offset: u64,
    count: Option<u64>,
    skipped: u64,
    emitted: u64,
}

impl Limit {
    pub fn new(child: Box<dyn PhysicalOperator>, offset: u64, count: Option<u64>) -> Self {
        Self { child, offset, count, skipped: 0, emitted: 0 }
    }
}

impl PhysicalOperator for Limit {
    fn name(&self) -> &'static str {
        "Limit"
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        vec![
            ("offset", self.offset.to_string()),
            (
                "count",
                self.count.map_or_else(|| "all".to_string(), |n| n.to_string()),
            ),
        ]
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
    }

    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.skipped = 0;
        self.emitted = 0;
        self.child.open(ctx)
    }

    fn next(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        while self.skipped < self.offset {
            if self.child.next(ctx)?.is_none() {
                return Ok(None);
            }
            self.skipped += 1;
        }
        if let Some(count) = self.count
            && self.emitted >= count
        {
            return Ok(None);
        }
        match self.child.next(ctx)? {
            Some(row) => {
                self.emitted += 1;
                Ok(Some(row))
            }
            None => Ok(None),
        }
    }

    fn close(&mut self) -> Result<()> {
        self.child.close()
    }
}

/// Keeps the first occurrence of each row; NULLs compare equal, matching the
/// materialized `distinct` semantics.
pub struct Distinct {
    child: Box<dyn PhysicalOperator>,
    seen: Vec<Vec<Value>>,
}

impl Distinct {
    pub fn new(child: Box<dyn PhysicalOperator>) -> Self {
        Self { child, seen: Vec::new() }
    }
}

impl PhysicalOperator for Distinct {
    fn name(&self) -> &'static str {
        "Distinct"
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
    }

    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.seen.clear();
        self.child.open(ctx)
    }

    fn next(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        while let Some(row) = self.child.next(ctx)? {
            if !self.seen.iter().any(|seen| seen == &row) {
                self.seen.push(row.clone());
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    fn close(&mut self) -> Result<()> {
        self.child.close()
    }
}

/// Blocking sort operator: materializes its child, orders by `order_by`
/// (resolving SELECT aliases against `items`), then streams the result.
pub struct Sort {
    child: Box<dyn PhysicalOperator>,
    order_by: Vec<(Expr, bool)>,
    items: Vec<SelectItem>,
    rows: Vec<Vec<Value>>,
    pos: usize,
}

impl Sort {
    pub fn new(
        child: Box<dyn PhysicalOperator>,
        order_by: Vec<(Expr, bool)>,
        items: Vec<SelectItem>,
    ) -> Self {
        Self { child, order_by, items, rows: Vec::new(), pos: 0 }
    }
}

impl PhysicalOperator for Sort {
    fn name(&self) -> &'static str {
        "Sort"
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        let keys: Vec<String> = self
            .order_by
            .iter()
            .map(|(e, desc)| format!("{e} {}", if *desc { "desc" } else { "asc" }))
            .collect();
        vec![("order by", keys.join(", "))]
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
    }

    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.child.open(ctx)?;
        let mut rows = Vec::new();
        while let Some(row) = self.child.next(ctx)? {
            rows.push(row);
        }
        self.child.close()?;
        let schema = self.child.schema().clone();
        sort_rows(
            ctx.db,
            ctx.trx,
            ctx.outer,
            &schema,
            &mut rows,
            &self.order_by,
            &self.items,
        )?;
        self.rows = rows;
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

/// Grouped/aggregate execution packaged as an operator. Materializes its
/// child, then delegates to the shared `grouped_select_rows` (group, having,
/// order groups, project, distinct, limit).
pub struct GroupBy {
    child: Box<dyn PhysicalOperator>,
    select: SelectStmt,
    exprs: Vec<Expr>,
    schema: Schema,
    rows: Vec<Vec<Value>>,
    pos: usize,
}

impl GroupBy {
    pub fn new(
        child: Box<dyn PhysicalOperator>,
        select: SelectStmt,
        exprs: Vec<Expr>,
        headers: Vec<String>,
    ) -> Self {
        let schema = Schema {
            columns: headers
                .into_iter()
                .map(|h| ColumnDesc::plain(None, h, DataType::Text))
                .collect(),
        };
        Self { child, select, exprs, schema, rows: Vec::new(), pos: 0 }
    }
}

impl PhysicalOperator for GroupBy {
    fn name(&self) -> &'static str {
        "GroupBy"
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        let groups: Vec<String> = self.exprs.iter().map(|e| e.to_string()).collect();
        vec![("groups", groups.join(", "))]
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.child.open(ctx)?;
        let schema = self.child.schema().clone();
        let mut columnar = None;
        if ctx.db.config().execution.mode == ExecutionMode::Chunk {
            columnar = if self.select.group_by.is_empty() {
                super::aggregate::chunk_global_aggregate(
                    ctx,
                    &schema,
                    self.child.as_mut(),
                    &self.select,
                    &self.exprs,
                )?
            } else {
                super::aggregate::chunk_grouped_aggregate(
                    ctx,
                    &schema,
                    self.child.as_mut(),
                    &self.select,
                    &self.exprs,
                )?
            };
        }
        self.rows = match columnar {
            Some(rows) => rows,
            None => {
                let mut filtered = Vec::new();
                while let Some(row) = self.child.next(ctx)? {
                    filtered.push(row);
                }
                super::aggregate::grouped_select_rows(
                    ctx.db,
                    ctx.trx,
                    ctx.outer,
                    &schema,
                    &self.select,
                    filtered,
                    self.exprs.clone(),
                )?
            }
        };
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

    fn next_chunk(&mut self, _ctx: &mut ExecContext<'_>) -> Result<Option<Chunk>> {
        if self.pos >= self.rows.len() {
            return Ok(None);
        }
        let end = (self.pos + CHUNK_ROWS).min(self.rows.len());
        let batch = Chunk::from_rows(&self.rows[self.pos..end])?;
        self.pos = end;
        Ok(Some(batch))
    }

    fn chunk_native(&self) -> bool {
        true
    }

    fn close(&mut self) -> Result<()> {
        self.rows.clear();
        self.pos = 0;
        Ok(())
    }
}

/// Left-deep nested-loop join. Materializes both children, then produces the
/// combined rows; LEFT/RIGHT keep unmatched rows with NULLs on the other side.
pub struct NestedLoopJoin {
    left: Box<dyn PhysicalOperator>,
    right: Box<dyn PhysicalOperator>,
    kind: JoinKind,
    condition: Option<Expr>,
    schema: Schema,
    rows: Vec<Vec<Value>>,
    pos: usize,
}

impl NestedLoopJoin {
    pub fn new(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        kind: JoinKind,
        condition: Option<Expr>,
    ) -> Result<Self> {
        if matches!(kind, JoinKind::Left | JoinKind::Right) && condition.is_none() {
            return Err(Error::Runtime("outer join requires an on clause".into()));
        }
        let mut schema = left.schema().clone();
        schema.columns.extend(right.schema().columns.iter().cloned());
        Ok(Self { left, right, kind, condition, schema, rows: Vec::new(), pos: 0 })
    }

    fn matches(&self, ctx: &mut ExecContext<'_>, row: &[Value]) -> Result<bool> {
        match &self.condition {
            None => Ok(true),
            Some(condition) => {
                eval_predicate_bound(ctx.db, ctx.trx, condition, &self.schema, row, ctx.outer)
            }
        }
    }
}

impl PhysicalOperator for NestedLoopJoin {
    fn name(&self) -> &'static str {
        "NestedLoopJoin"
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        let mut out = vec![("join", format!("{:?}", self.kind))];
        if let Some(condition) = &self.condition {
            out.push(("condition", condition.to_string()));
        }
        out
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.left.as_ref(), self.right.as_ref()]
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        let left_rows = drain(&mut self.left, ctx)?;
        let right_rows = drain(&mut self.right, ctx)?;
        let left_cols = self.left.schema().columns.len();
        let right_cols = self.right.schema().columns.len();

        let mut rows = Vec::new();
        match self.kind {
            JoinKind::Left => {
                for left in &left_rows {
                    let mut matched = false;
                    for right in &right_rows {
                        let mut row = left.clone();
                        row.extend(right.iter().cloned());
                        if self.matches(ctx, &row)? {
                            rows.push(row);
                            matched = true;
                        }
                    }
                    if !matched {
                        let mut row = left.clone();
                        row.extend(vec![Value::Null; right_cols]);
                        rows.push(row);
                    }
                }
            }
            JoinKind::Right => {
                for right in &right_rows {
                    let mut matched = false;
                    for left in &left_rows {
                        let mut row = left.clone();
                        row.extend(right.iter().cloned());
                        if self.matches(ctx, &row)? {
                            rows.push(row);
                            matched = true;
                        }
                    }
                    if !matched {
                        let mut row = vec![Value::Null; left_cols];
                        row.extend(right.iter().cloned());
                        rows.push(row);
                    }
                }
            }
            JoinKind::Cross | JoinKind::Inner => {
                for left in &left_rows {
                    for right in &right_rows {
                        let mut row = left.clone();
                        row.extend(right.iter().cloned());
                        if self.matches(ctx, &row)? {
                            rows.push(row);
                        }
                    }
                }
            }
        }
        self.rows = rows;
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

/// Opens `op`, collects all its rows, and closes it.
fn drain(op: &mut Box<dyn PhysicalOperator>, ctx: &mut ExecContext<'_>) -> Result<Vec<Vec<Value>>> {
    op.open(ctx)?;
    let mut rows = Vec::new();
    while let Some(row) = op.next(ctx)? {
        rows.push(row);
    }
    op.close()?;
    Ok(rows)
}

/// Opens `op`, appends all its rows to `out` flat (row-major), and closes it.
/// The fused row path reuses one row buffer, so no `Vec` is allocated per row.
fn drain_into(
    op: &mut Box<dyn PhysicalOperator>,
    ctx: &mut ExecContext<'_>,
    out: &mut Vec<Value>,
) -> Result<()> {
    op.open(ctx)?;
    let fused = op.for_each_row(ctx, &mut |row| {
        out.extend_from_slice(row);
        Ok(())
    })?;
    if !fused {
        while let Some(row) = op.next(ctx)? {
            out.extend_from_slice(&row);
        }
    }
    op.close()
}

/// Dtype of a simple column reference, used to reject hash keys whose numerics
/// would need coercion (the index key encoding is type-sensitive).
fn column_dtype(schema: &Schema, expr: &Expr) -> Option<DataType> {
    match expr {
        Expr::Column(name) => {
            schema.columns.iter().find(|c| &c.name == name).map(|c| c.dtype)
        }
        Expr::QualifiedColumn(owner, name) => schema
            .columns
            .iter()
            .find(|c| c.owner.as_deref() == Some(owner) && &c.name == name)
            .map(|c| c.dtype),
        _ => None,
    }
}

fn compatible(a: DataType, b: DataType) -> bool {
    matches!(
        (a, b),
        (DataType::Int, DataType::Int)
            | (DataType::Float, DataType::Float)
            | (DataType::Date, DataType::Date)
            | (DataType::Text, DataType::Text)
            | (DataType::Char(_), DataType::Char(_))
    )
}

fn split_conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Binary(BinOp::And, l, r) => {
            let mut out = split_conjuncts(l);
            out.extend(split_conjuncts(r));
            out
        }
        other => vec![other],
    }
}

fn combine_and(mut parts: Vec<Expr>) -> Option<Expr> {
    let mut acc = parts.pop()?;
    while let Some(e) = parts.pop() {
        acc = Expr::Binary(BinOp::And, Box::new(e), Box::new(acc));
    }
    Some(acc)
}

pub struct HashKeys {
    left_keys: Vec<Expr>,
    right_keys: Vec<Expr>,
    residual: Option<Expr>,
}

/// Extracts equi-join key pairs from `ON a.k = b.k [and ...]`. Returns `None`
/// unless at least one pair of plain, same-typed columns is found.
fn analyze_hash_join(
    kind: JoinKind,
    condition: Option<&Expr>,
    left: &Schema,
    right: &Schema,
) -> Option<HashKeys> {
    if kind == JoinKind::Cross {
        return None;
    }
    let condition = condition?;
    let conjuncts: Vec<Expr> = split_conjuncts(condition).into_iter().cloned().collect();
    let (left_keys, right_keys, residual) = extract_hash_keys(&conjuncts, left, right);
    if left_keys.is_empty() {
        return None;
    }
    Some(HashKeys { left_keys, right_keys, residual: combine_and(residual) })
}

/// Splits `conjuncts` into equi-join key pairs between `left` and `right`, plus
/// the predicates that must still be evaluated. A conjunct becomes a key only
/// when its two sides resolve to compatible columns of the respective schemas,
/// so callers can safely drop the returned keys from a filter.
fn extract_hash_keys(
    conjuncts: &[Expr],
    left: &Schema,
    right: &Schema,
) -> (Vec<Expr>, Vec<Expr>, Vec<Expr>) {
    let mut left_keys = Vec::new();
    let mut right_keys = Vec::new();
    let mut kept = Vec::new();
    for conjunct in conjuncts {
        if let Expr::Binary(BinOp::Eq, a, b) = conjunct {
            if let (Some(ld), Some(rd)) = (column_dtype(left, a), column_dtype(right, b))
                && compatible(ld, rd)
            {
                left_keys.push((**a).clone());
                right_keys.push((**b).clone());
                continue;
            }
            if let (Some(ld), Some(rd)) = (column_dtype(left, b), column_dtype(right, a))
                && compatible(ld, rd)
            {
                left_keys.push((**b).clone());
                right_keys.push((**a).clone());
                continue;
            }
        }
        kept.push(conjunct.clone());
    }
    (left_keys, right_keys, kept)
}

/// The join kind and optional ON clause for every table after the first, in
/// order. A comma join is `Cross` with no ON; an explicit join consumes the
/// next entry of `select.on`, keeping both vectors aligned even when commas
/// and explicit joins are mixed.
fn join_clauses(select: &SelectStmt) -> Vec<(JoinKind, Option<Expr>)> {
    let mut out = Vec::new();
    let mut on_index = 0usize;
    for i in 1..select.from.len() {
        let kind = select.joins.get(i).copied().unwrap_or(JoinKind::Cross);
        let on = if kind == JoinKind::Cross {
            None
        } else {
            let on = select.on.get(on_index).cloned();
            on_index += 1;
            on
        };
        out.push((kind, on));
    }
    out
}

/// Whether the left-deep join chain hashes at least one pair. Lets `EXPLAIN`
/// report a comma join rewritten onto WHERE equi-keys as a `HashJoin`.
pub(crate) fn select_uses_hash_join(db: &Database, s: &SelectStmt) -> Result<bool> {
    if s.from.len() < 2 {
        return Ok(false);
    }
    let Some(first) = build_from_source(db, &s.from[0])? else {
        return Ok(false);
    };
    let mut left_schema = first.schema().clone();
    let mut where_conjuncts: Vec<Expr> =
        s.selection.as_ref().map(split_conjuncts).unwrap_or_default()
            .into_iter().cloned().collect();
    for (i, (kind, on)) in join_clauses(s).into_iter().enumerate() {
        let Some(right) = build_from_source(db, &s.from[i + 1])? else {
            return Ok(false);
        };
        let right_schema = right.schema().clone();
        let hashed = match on.as_ref() {
            Some(on) => analyze_hash_join(kind, Some(on), &left_schema, &right_schema).is_some(),
            None if kind == JoinKind::Cross => {
                let (left_keys, _, kept) =
                    extract_hash_keys(&where_conjuncts, &left_schema, &right_schema);
                if left_keys.is_empty() {
                    false
                } else {
                    where_conjuncts = kept;
                    true
                }
            }
            None => false,
        };
        if hashed {
            return Ok(true);
        }
        left_schema.columns.extend(right_schema.columns.iter().cloned());
    }
    Ok(false)
}

/// Appends one key component to `out`, length-prefixed so components cannot
/// run together. Returns `false` for a NULL value (the key never matches).
fn push_key_component(out: &mut Vec<u8>, value: &Value) -> Result<bool> {
    if matches!(value, Value::Null) {
        return Ok(false);
    }
    let start = out.len();
    out.extend_from_slice(&[0u8; 4]); // length prefix, patched below
    encode_key_into(out, value)?;
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
    Ok(true)
}

/// Encodes the columns at `indices` into `out` (cleared); `false` on a NULL.
fn indices_key_into(indices: &[usize], row: &[Value], out: &mut Vec<u8>) -> Result<bool> {
    out.clear();
    for &index in indices {
        if !push_key_component(out, &row[index])? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Encodes expression keys into `out` (cleared); `false` on a NULL value.
fn expr_key_into(
    keys: &[Expr],
    schema: &Schema,
    row: &[Value],
    db: &Database,
    trx: &mut crate::trx::TrxState,
    out: &mut Vec<u8>,
) -> Result<bool> {
    out.clear();
    let ctx = EvalCtx::row(schema, row);
    for key in keys {
        let value = eval_bound(db, trx, key, Some(&ctx))?;
        if !push_key_component(out, &value)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Resolves hash-join keys that are plain columns to their schema positions.
fn key_indices(keys: &[Expr], schema: &Schema) -> Option<Vec<usize>> {
    let mut indices = Vec::with_capacity(keys.len());
    for key in keys {
        let index = match key {
            Expr::Column(name) => schema.columns.iter().position(|c| &c.name == name)?,
            Expr::QualifiedColumn(owner, name) => schema
                .columns
                .iter()
                .position(|c| c.owner.as_deref() == Some(owner) && &c.name == name)?,
            _ => return None,
        };
        indices.push(index);
    }
    Some(indices)
}

/// Hash equi-join: builds a hash table on one side, then streams the other
/// side in chunks, preserving row order. Supports INNER/LEFT (build right)
/// and RIGHT (build left).
pub struct HashJoin {
    left: Box<dyn PhysicalOperator>,
    right: Box<dyn PhysicalOperator>,
    kind: JoinKind,
    left_keys: Vec<Expr>,
    right_keys: Vec<Expr>,
    residual: Option<Expr>,
    schema: Schema,
    table: HashMap<Vec<u8>, MatchList>,
    /// Build rows stored flat (row-major, `build_cols` per row) so building the
    /// table does not allocate a `Vec` per row.
    build_values: Vec<Value>,
    build_cols: usize,
    probe_is_right: bool,
    probe_keys: Vec<Expr>,
    probe_schema: Schema,
    probe_key_indices: Option<Vec<usize>>,
    probe_open: bool,
    probe_done: bool,
    pending: VecDeque<Vec<Value>>,
    left_cols: usize,
    right_cols: usize,
}

impl HashJoin {
    pub fn new(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        kind: JoinKind,
        keys: HashKeys,
    ) -> Self {
        let mut schema = left.schema().clone();
        schema.columns.extend(right.schema().columns.iter().cloned());
        Self {
            left,
            right,
            kind,
            left_keys: keys.left_keys,
            right_keys: keys.right_keys,
            residual: keys.residual,
            schema,
            table: HashMap::new(),
            build_values: Vec::new(),
            build_cols: 0,
            probe_is_right: false,
            probe_keys: Vec::new(),
            probe_schema: Schema::default(),
            probe_key_indices: None,
            probe_open: false,
            probe_done: false,
            pending: VecDeque::new(),
            left_cols: 0,
            right_cols: 0,
        }
    }

    /// The build row at `index`; build rows are stored flat with a fixed
    /// `build_cols` stride into `build_values`.
    fn build_row(&self, index: usize) -> &[Value] {
        let start = index * self.build_cols;
        &self.build_values[start..start + self.build_cols]
    }

    /// Encodes one probe row's join key into `out` (cleared); `false` means a
    /// NULL key (never matches).
    fn probe_key_into(
        &self,
        chunk: &Chunk,
        i: usize,
        ctx: &mut ExecContext<'_>,
        out: &mut Vec<u8>,
    ) -> Result<bool> {
        match &self.probe_key_indices {
            Some(indices) => {
                out.clear();
                for &index in indices {
                    let value = chunk.column(index).value(i);
                    if !push_key_component(out, &value)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            None => {
                let row = chunk.row(i);
                expr_key_into(&self.probe_keys, &self.probe_schema, &row, ctx.db, ctx.trx, out)
            }
        }
    }

    /// Pulls one probe chunk and appends its joined rows to `pending`.
    /// Returns `false` once the probe side is exhausted.
    fn fill_output(&mut self, ctx: &mut ExecContext<'_>) -> Result<bool> {
        if self.probe_done {
            return Ok(false);
        }
        if !self.probe_open {
            if self.probe_is_right {
                self.right.open(ctx)?;
            } else {
                self.left.open(ctx)?;
            }
            self.probe_open = true;
        }
        let pulled = {
            let probe: &mut Box<dyn PhysicalOperator> =
                if self.probe_is_right { &mut self.right } else { &mut self.left };
            probe.next_chunk(ctx)?
        };
        let Some(chunk) = pulled else {
            let probe: &mut Box<dyn PhysicalOperator> =
                if self.probe_is_right { &mut self.right } else { &mut self.left };
            probe.close()?;
            self.probe_open = false;
            self.probe_done = true;
            return Ok(false);
        };

        let outer = if self.probe_is_right {
            self.kind == JoinKind::Right
        } else {
            self.kind == JoinKind::Left
        };
        let mut key_buf = Vec::new();
        let mut match_buf: Vec<usize> = Vec::new();
        for i in 0..chunk.len() {
            let has_key = self.probe_key_into(&chunk, i, ctx, &mut key_buf)?;
            match_buf.clear();
            if has_key && let Some(list) = self.table.get(&key_buf) {
                list.copy_into(&mut match_buf);
            }
            if match_buf.is_empty() && !outer {
                continue;
            }
            let row = chunk.row(i);
            let mut matched = false;
            if !match_buf.is_empty() {
                for &build_index in &match_buf {
                    let mut combined = if self.probe_is_right {
                        self.build_row(build_index).to_vec()
                    } else {
                        row.clone()
                    };
                    if self.probe_is_right {
                        combined.extend(row.iter().cloned());
                    } else {
                        combined.extend_from_slice(self.build_row(build_index));
                    }
                    if residual_ok(&self.residual, &self.schema, ctx, &combined)? {
                        self.pending.push_back(combined);
                        matched = true;
                    }
                }
            }
            if !matched && outer {
                let combined = if self.probe_is_right {
                    let mut nulls = vec![Value::Null; self.left_cols];
                    nulls.extend(row.iter().cloned());
                    nulls
                } else {
                    let mut left = row.clone();
                    left.extend(vec![Value::Null; self.right_cols]);
                    left
                };
                self.pending.push_back(combined);
            }
        }
        Ok(true)
    }
}

fn residual_ok(
    residual: &Option<Expr>,
    schema: &Schema,
    ctx: &mut ExecContext<'_>,
    row: &[Value],
) -> Result<bool> {
    match residual {
        None => Ok(true),
        Some(predicate) => {
            eval_predicate_bound(ctx.db, ctx.trx, predicate, schema, row, ctx.outer)
        }
    }
}

impl PhysicalOperator for HashJoin {
    fn name(&self) -> &'static str {
        "HashJoin"
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        let keys: Vec<String> = self
            .left_keys
            .iter()
            .zip(&self.right_keys)
            .map(|(l, r)| format!("{l} = {r}"))
            .collect();
        let mut out = vec![("join", format!("{:?}", self.kind)), ("keys", keys.join(", "))];
        if let Some(residual) = &self.residual {
            out.push(("residual", residual.to_string()));
        }
        out
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.left.as_ref(), self.right.as_ref()]
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.pending.clear();
        self.table.clear();
        self.build_values.clear();
        self.probe_open = false;
        self.probe_done = false;
        self.left_cols = self.left.schema().columns.len();
        self.right_cols = self.right.schema().columns.len();

        let build_is_right = self.kind != JoinKind::Right;
        self.probe_is_right = !build_is_right;

        let (build_schema, build_keys) = if build_is_right {
            (self.right.schema().clone(), self.right_keys.clone())
        } else {
            (self.left.schema().clone(), self.left_keys.clone())
        };
        self.build_cols = build_schema.columns.len();
        {
            let build_op = if build_is_right { &mut self.right } else { &mut self.left };
            drain_into(build_op, ctx, &mut self.build_values)?;
        }
        let build_key_indices = key_indices(&build_keys, &build_schema);
        let rows = self.build_values.len().checked_div(self.build_cols).unwrap_or(0);
        self.table.reserve(rows);
        let mut key_buf = Vec::new();
        for i in 0..rows {
            let row = &self.build_values[i * self.build_cols..(i + 1) * self.build_cols];
            let has_key = match &build_key_indices {
                Some(indices) => indices_key_into(indices, row, &mut key_buf)?,
                None => {
                    expr_key_into(&build_keys, &build_schema, row, ctx.db, ctx.trx, &mut key_buf)?
                }
            };
            if has_key {
                self.table
                    .entry(key_buf.clone())
                    .and_modify(|list| list.push(i))
                    .or_insert(MatchList::One(i));
            }
        }

        if self.probe_is_right {
            self.probe_keys = self.right_keys.clone();
            self.probe_schema = self.right.schema().clone();
        } else {
            self.probe_keys = self.left_keys.clone();
            self.probe_schema = self.left.schema().clone();
        }
        self.probe_key_indices = key_indices(&self.probe_keys, &self.probe_schema);
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        loop {
            if let Some(row) = self.pending.pop_front() {
                return Ok(Some(row));
            }
            if !self.fill_output(ctx)? {
                return Ok(None);
            }
        }
    }

    fn next_chunk(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Chunk>> {
        loop {
            if !self.pending.is_empty() {
                let take = self.pending.len().min(CHUNK_ROWS);
                let rows: Vec<Vec<Value>> = self.pending.drain(..take).collect();
                return Ok(Some(Chunk::from_rows(&rows)?));
            }
            if !self.fill_output(ctx)? {
                return Ok(None);
            }
        }
    }

    fn for_each_row(
        &mut self,
        ctx: &mut ExecContext<'_>,
        sink: &mut dyn FnMut(&[Value]) -> Result<()>,
    ) -> Result<bool> {
        if self.probe_done {
            return Ok(true);
        }
        if !self.probe_open {
            let probe: &mut Box<dyn PhysicalOperator> =
                if self.probe_is_right { &mut self.right } else { &mut self.left };
            probe.open(ctx)?;
            self.probe_open = true;
        }
        let outer = if self.probe_is_right {
            self.kind == JoinKind::Right
        } else {
            self.kind == JoinKind::Left
        };
        let probe_cols = self.probe_schema.columns.len();
        let mut key_buf = Vec::new();
        let mut match_buf: Vec<usize> = Vec::new();
        let mut combined: Vec<Value> = Vec::with_capacity(self.left_cols + self.right_cols);
        loop {
            let pulled = {
                let probe: &mut Box<dyn PhysicalOperator> =
                    if self.probe_is_right { &mut self.right } else { &mut self.left };
                probe.next_chunk(ctx)?
            };
            let Some(chunk) = pulled else { break };
            for i in 0..chunk.len() {
                let has_key = self.probe_key_into(&chunk, i, ctx, &mut key_buf)?;
                match_buf.clear();
                if has_key && let Some(list) = self.table.get(&key_buf) {
                    list.copy_into(&mut match_buf);
                }
                if match_buf.is_empty() && !outer {
                    continue;
                }
                let mut matched = false;
                if !match_buf.is_empty() {
                    for &build_index in &match_buf {
                        combined.clear();
                        if self.probe_is_right {
                            combined.extend_from_slice(self.build_row(build_index));
                            for c in 0..probe_cols {
                                combined.push(chunk.column(c).value(i));
                            }
                        } else {
                            for c in 0..probe_cols {
                                combined.push(chunk.column(c).value(i));
                            }
                            combined.extend_from_slice(self.build_row(build_index));
                        }
                        if residual_ok(&self.residual, &self.schema, ctx, &combined)? {
                            sink(&combined)?;
                            matched = true;
                        }
                    }
                }
                if !matched && outer {
                    combined.clear();
                    if self.probe_is_right {
                        combined.extend(std::iter::repeat_n(Value::Null, self.left_cols));
                        for c in 0..probe_cols {
                            combined.push(chunk.column(c).value(i));
                        }
                    } else {
                        for c in 0..probe_cols {
                            combined.push(chunk.column(c).value(i));
                        }
                        combined.extend(std::iter::repeat_n(Value::Null, self.right_cols));
                    }
                    sink(&combined)?;
                }
            }
        }
        if self.probe_open {
            let probe: &mut Box<dyn PhysicalOperator> =
                if self.probe_is_right { &mut self.right } else { &mut self.left };
            probe.close()?;
            self.probe_open = false;
        }
        self.probe_done = true;
        Ok(true)
    }

    fn close(&mut self) -> Result<()> {
        if self.probe_open {
            let probe: &mut Box<dyn PhysicalOperator> =
                if self.probe_is_right { &mut self.right } else { &mut self.left };
            probe.close()?;
            self.probe_open = false;
        }
        self.table.clear();
        self.build_values.clear();
        self.pending.clear();
        Ok(())
    }
}

/// UNION [ALL] over a list of plans, then the trailing ORDER BY / LIMIT that
/// apply to the whole set. `(true, plan)` marks a UNION ALL operand.
pub struct Union {
    inputs: Vec<(bool, Box<dyn PhysicalOperator>)>,
    schema: Schema,
    rows: Vec<Vec<Value>>,
    pos: usize,
    order_by: Vec<(Expr, bool)>,
    limit: Option<LimitClause>,
}

impl Union {
    pub fn new(
        inputs: Vec<(bool, Box<dyn PhysicalOperator>)>,
        order_by: Vec<(Expr, bool)>,
        limit: Option<LimitClause>,
    ) -> Self {
        let headers: Vec<String> =
            inputs[0].1.schema().columns.iter().map(|c| c.name.clone()).collect();
        let schema = Schema {
            columns: headers
                .into_iter()
                .map(|h| ColumnDesc::plain(None, h, DataType::Text))
                .collect(),
        };
        Self { inputs, schema, rows: Vec::new(), pos: 0, order_by, limit }
    }
}

impl PhysicalOperator for Union {
    fn name(&self) -> &'static str {
        "Union"
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        let mut out = vec![("inputs", self.inputs.len().to_string())];
        if !self.order_by.is_empty() {
            let keys: Vec<String> = self
                .order_by
                .iter()
                .map(|(e, desc)| format!("{e} {}", if *desc { "desc" } else { "asc" }))
                .collect();
            out.push(("order by", keys.join(", ")));
        }
        if let Some(limit) = &self.limit {
            let offset = limit.offset.as_ref().map_or_else(|| "0".to_string(), |e| e.to_string());
            out.push(("limit", format!("offset {} count {}", offset, limit.count)));
        }
        out
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        self.inputs.iter().map(|(_, op)| op.as_ref()).collect()
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        let columns: Vec<String> =
            self.schema.columns.iter().map(|c| c.name.clone()).collect();
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for (i, (all, plan)) in self.inputs.iter_mut().enumerate() {
            let mut part = drain(plan, ctx)?;
            if i == 0 {
                rows = part;
                continue;
            }
            if plan.schema().columns.len() != columns.len() {
                return Err(Error::Runtime(format!(
                    "union column count mismatch: {} vs {}",
                    columns.len(),
                    plan.schema().columns.len()
                )));
            }
            rows.append(&mut part);
            if !*all {
                super::aggregate::dedup_rows(&mut rows);
            }
        }
        if !self.order_by.is_empty() {
            super::sort_projected(
                ctx.db,
                ctx.trx,
                ctx.outer,
                &columns,
                &mut rows,
                &self.order_by,
            )?;
        }
        super::aggregate::apply_limit(&mut rows, &self.limit)?;
        self.rows = rows;
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

/// Index scan: fetches exactly the row ids the access path selected.
pub struct IndexScan {
    table: String,
    schema: Schema,
    engine: std::sync::Arc<dyn crate::storage::engine::TableStorage>,
    column: String,
    rids: Vec<Rid>,
    /// A lazy in-order cursor over the index; when set, `rids` is unused.
    cursor: Option<crate::index::LeafCursor>,
    pos: usize,
}

impl IndexScan {
    /// Returns `None` when the selection is not sargable (use a `TableScan`).
    pub fn new(db: &Database, table: &str, selection: Option<&Expr>) -> Result<Option<Self>> {
        Self::with_owner(db, table, table, selection)
    }

    /// `owner` is the alias (or table name) that qualifies this scan's columns.
    pub fn with_owner(
        db: &Database,
        table: &str,
        owner: &str,
        selection: Option<&Expr>,
    ) -> Result<Option<Self>> {
        let Some(plan) = crate::exec::plan::plan_index_scan(db, table, selection)? else {
            return Ok(None);
        };
        let mut scan = Self::skeleton(db, table, owner, plan.column)?;
        scan.rids = plan.rids;
        Ok(Some(scan))
    }

    /// Scans `column`'s index in ascending key order, so an `ORDER BY column`
    /// can reuse the index instead of sorting. The scan is lazy, so a LIMIT
    /// stops it once it has the rows it needs. Returns `None` without that
    /// index.
    pub fn ordered(db: &Database, table: &str, owner: &str, column: &str) -> Result<Option<Self>> {
        let Some(file) = crate::exec::plan::ordered_index_file(db, table, column)? else {
            return Ok(None);
        };
        let mut scan = Self::skeleton(db, table, owner, column.to_string())?;
        scan.cursor = Some(crate::index::BTree::at(file).leaf_cursor(&db.pool)?);
        Ok(Some(scan))
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
            rids: Vec::new(),
            cursor: None,
            pos: 0,
        })
    }

    /// The indexed column, which the scan yields in ascending order.
    pub fn ordered_column(&self) -> &str {
        &self.column
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
    fn name(&self) -> &'static str {
        "IndexScan"
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        vec![
            ("table", self.table.clone()),
            ("column", self.column.clone()),
            // The index name is not stored here (execution only needs the
            // column); the planner's `chosen` record carries it.
            (
                "access",
                match self.cursor.is_some() {
                    true => "ordered leaf scan",
                    false => "index lookup",
                }
                .to_string(),
            ),
        ]
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        ctx.db.note_read(ctx.trx.id, &self.table);
        self.pos = 0;
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
        Ok(())
    }
}

/// Builds a plan for statements the operator layer covers: SELECT and DML.
/// Other statements (DDL, EXPLAIN, transaction control) return `None`.
pub fn build_statement(
    db: &Database,
    stmt: &Stmt,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    match stmt {
        Stmt::Select(select) => build_select(db, select),
        Stmt::Insert(insert) => {
            Ok(Some(Box::new(crate::exec::dml::InsertOp::new(insert.clone()))))
        }
        Stmt::Update(update) => {
            Ok(Some(Box::new(crate::exec::dml::UpdateOp::new(update.clone()))))
        }
        Stmt::Delete(delete) => {
            Ok(Some(Box::new(crate::exec::dml::DeleteOp::new(delete.clone()))))
        }
        _ => Ok(None),
    }
}

/// Builds a scan for one FROM entry: a table scan or a view sub-plan.
fn build_from_source(
    db: &Database,
    tref: &TableRef,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    let owner = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
    let view_sql = db.catalog().view(&tref.name).cloned();
    if let Some(sql) = view_sql {
        return Ok(ViewScan::new(db, &sql, &owner)?
            .map(|scan| Box::new(scan) as Box<dyn PhysicalOperator>));
    }
    Ok(Some(Box::new(TableScan::with_owner(db, &tref.name, &owner)?)))
}

/// Builds a physical plan for a SELECT. Returns `None` for any shape the
/// operators do not cover yet, leaving the materialized executor as fallback.
pub fn build_select(
    db: &Database,
    select: &SelectStmt,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    // set operations: build each operand, then apply the trailing order/limit
    if !select.set_ops.is_empty() {
        return build_set_op(db, select);
    }

    // no FROM: a single projected tuple (distinct/order/limit are ignored by
    // the materialized path here, so we match that)
    if select.from.is_empty() {
        if select.items.iter().any(|it| matches!(it, SelectItem::Star))
            || items_have_aggregate(&select.items)
        {
            return Ok(None);
        }
        let (exprs, headers) = projection_exprs(&select.items);
        let plan = Project::new(Box::new(ConstantScan::new()), exprs, headers);
        return Ok(Some(Box::new(plan)));
    }

    // FROM: a single table uses the best access path; multiple tables build a
    // left-deep nested-loop/hash join over from-sources (tables or views).
    let mut post_filter = select.selection.clone();
    let (mut op, ordered_by): (Box<dyn PhysicalOperator>, Option<String>) = if select.from.len() == 1
    {
        let tref = &select.from[0];
        if db.catalog().view(&tref.name).is_some() {
            let Some(source) = build_from_source(db, tref)? else {
                return Ok(None);
            };
            (source, None)
        } else {
            let owner = tref.alias.as_deref().unwrap_or(&tref.name);
            match IndexScan::with_owner(db, &tref.name, owner, select.selection.as_ref())? {
                Some(scan) => {
                    let column = scan.ordered_column().to_string();
                    (Box::new(scan), Some(column))
                }
                None => {
                    // An index on the ORDER BY column can supply the order even
                    // without a WHERE clause, skipping the sort.
                    if select.group_by.is_empty()
                        && !items_have_aggregate(&select.items)
                        && let Some(column) =
                            crate::exec::plan::resolved_order_column(&select.items, &select.order_by)
                        && let Some(scan) =
                            IndexScan::ordered(db, &tref.name, owner, &column)?
                    {
                        let column = scan.ordered_column().to_string();
                        (Box::new(scan), Some(column))
                    } else {
                        // sequential scans may skip large objects the query never reads
                        let keep = lob_keep(
                            select,
                            &db.catalog().table(&tref.name)?.schema.columns,
                            owner,
                            &tref.name,
                        );
                        (Box::new(TableScan::with_owner_keep(db, &tref.name, owner, keep)?), None)
                    }
                }
            }
        }
    } else {
        let Some(mut op) = build_from_source(db, &select.from[0])? else {
            return Ok(None);
        };
        // Comma joins carry no ON clause; equi-predicates between the two sides
        // are sourced from WHERE so the join can hash instead of cross-produce.
        let mut where_conjuncts: Vec<Expr> =
            select.selection.as_ref().map(split_conjuncts).unwrap_or_default()
                .into_iter().cloned().collect();
        let mut where_reduced = false;
        for (i, (kind, on)) in join_clauses(select).into_iter().enumerate() {
            let Some(right) = build_from_source(db, &select.from[i + 1])? else {
                return Ok(None);
            };
            let keys = match on.as_ref() {
                Some(on) => analyze_hash_join(kind, Some(on), op.schema(), right.schema()),
                None if kind == JoinKind::Cross => {
                    let (left_keys, right_keys, kept) =
                        extract_hash_keys(&where_conjuncts, op.schema(), right.schema());
                    if left_keys.is_empty() {
                        None
                    } else {
                        where_conjuncts = kept;
                        where_reduced = true;
                        Some(HashKeys { left_keys, right_keys, residual: None })
                    }
                }
                None => None,
            };
            match keys {
                Some(keys) => op = Box::new(HashJoin::new(op, right, kind, keys)),
                None => op = Box::new(NestedLoopJoin::new(op, right, kind, on)?),
            }
        }
        if where_reduced {
            post_filter = combine_and(where_conjuncts);
        }
        (op, None)
    };
    if let Some(selection) = &post_filter {
        op = Box::new(Filter::new(op, selection.clone()));
    }

    // grouped / aggregate: grouping, having, ordering, projection, distinct
    // and limit are all handled inside GroupBy (matching the materialized path)
    if !select.group_by.is_empty()
        || select.having.is_some()
        || items_have_aggregate(&select.items)
    {
        let (exprs, headers) = build_projection(op.schema(), &select.items);
        return Ok(Some(Box::new(GroupBy::new(op, select.clone(), exprs, headers))));
    }

    if !select.order_by.is_empty() {
        // an ascending scan on the ordering column already yields the order
        let skip = ordered_by.as_deref().is_some_and(|column| {
            crate::exec::plan::resolved_order_column(&select.items, &select.order_by).as_deref()
                == Some(column)
        });
        if !skip {
            op = Box::new(Sort::new(op, select.order_by.clone(), select.items.clone()));
        }
    }
    let (exprs, headers) = build_projection(op.schema(), &select.items);
    op = Box::new(Project::new(op, exprs, headers));
    if select.distinct {
        op = Box::new(Distinct::new(op));
    }
    if let Some(limit) = &select.limit {
        let offset = limit_bound(limit.offset.as_ref())?;
        let count = limit_bound(Some(&limit.count))?;
        op = Box::new(Limit::new(op, offset, Some(count)));
    }
    Ok(Some(op))
}

/// Builds the plan for a UNION [ALL] chain: each operand is planned, then the
/// trailing ORDER BY / LIMIT apply to the whole result.
fn build_set_op(
    db: &Database,
    select: &SelectStmt,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    let mut base = select.clone();
    base.set_ops = Vec::new();
    let order_by = std::mem::take(&mut base.order_by);
    let limit = base.limit.take();
    let Some(base_plan) = build_select(db, &base)? else {
        return Ok(None);
    };
    let mut inputs: Vec<(bool, Box<dyn PhysicalOperator>)> = vec![(true, base_plan)];
    for (all, operand) in &select.set_ops {
        let Some(plan) = build_select(db, operand)? else {
            return Ok(None);
        };
        inputs.push((*all, plan));
    }
    Ok(Some(Box::new(Union::new(inputs, order_by, limit))))
}

fn projection_exprs(items: &[SelectItem]) -> (Vec<Expr>, Vec<String>) {
    let mut exprs = Vec::new();
    let mut headers = Vec::new();
    for item in items {
        match item {
            SelectItem::Expr(e) => {
                headers.push(e.to_string());
                exprs.push(e.clone());
            }
            SelectItem::Aliased(e, alias) => {
                headers.push(alias.clone());
                exprs.push(e.clone());
            }
            SelectItem::Star => unreachable!("star is rejected before projection"),
        }
    }
    (exprs, headers)
}

/// Expands SELECT items against `schema`: `*` becomes owner-qualified column
/// references so joins stay unambiguous.
fn build_projection(schema: &Schema, items: &[SelectItem]) -> (Vec<Expr>, Vec<String>) {
    let mut exprs = Vec::new();
    let mut headers = Vec::new();
    for item in items {
        match item {
            SelectItem::Star => {
                for column in &schema.columns {
                    headers.push(column.name.clone());
                    exprs.push(match &column.owner {
                        Some(owner) => Expr::QualifiedColumn(owner.clone(), column.name.clone()),
                        None => Expr::Column(column.name.clone()),
                    });
                }
            }
            SelectItem::Expr(e) => {
                headers.push(e.to_string());
                exprs.push(e.clone());
            }
            SelectItem::Aliased(e, alias) => {
                headers.push(alias.clone());
                exprs.push(e.clone());
            }
        }
    }
    (exprs, headers)
}

/// Per-column flags marking which base columns a single-table SELECT reads.
/// Returns `None` (do not prune) whenever the analysis cannot be certain:
/// multiple sources, a `*` projection, a subquery, or a foreign qualifier.
fn lob_keep(
    select: &SelectStmt,
    columns: &[ColumnDesc],
    owner: &str,
    table: &str,
) -> Option<Vec<bool>> {
    if select.from.len() != 1 {
        return None;
    }
    let mut needed: HashSet<String> = HashSet::new();
    let mut safe = true;
    let visit = |expr: &Expr, needed: &mut HashSet<String>, safe: &mut bool| {
        if !collect_column_refs(expr, owner, table, needed) {
            *safe = false;
        }
    };
    for item in &select.items {
        match item {
            SelectItem::Star => return None,
            SelectItem::Expr(e) | SelectItem::Aliased(e, _) => {
                visit(e, &mut needed, &mut safe);
            }
        }
    }
    if let Some(selection) = &select.selection {
        visit(selection, &mut needed, &mut safe);
    }
    for expr in &select.group_by {
        visit(expr, &mut needed, &mut safe);
    }
    if let Some(having) = &select.having {
        visit(having, &mut needed, &mut safe);
    }
    for (expr, _) in &select.order_by {
        visit(expr, &mut needed, &mut safe);
    }
    if !safe {
        return None;
    }
    Some(columns.iter().map(|c| needed.contains(&c.name)).collect())
}

/// Records base-column references; returns `false` when it sees something it
/// cannot classify (a subquery or a foreign qualifier), forcing full decoding.
fn collect_column_refs(expr: &Expr, owner: &str, table: &str, needed: &mut HashSet<String>) -> bool {
    match expr {
        Expr::Column(name) => {
            needed.insert(name.clone());
            true
        }
        Expr::QualifiedColumn(qual, name) => {
            if qual == owner || qual == table {
                needed.insert(name.clone());
                true
            } else {
                false
            }
        }
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Null | Expr::Value(_) => true,
        Expr::Unary(_, a) => collect_column_refs(a, owner, table, needed),
        Expr::Binary(_, a, b) => {
            collect_column_refs(a, owner, table, needed)
                & collect_column_refs(b, owner, table, needed)
        }
        Expr::IsNull(a, _) => collect_column_refs(a, owner, table, needed),
        Expr::Like { expr, pattern, .. } => {
            collect_column_refs(expr, owner, table, needed)
                & collect_column_refs(pattern, owner, table, needed)
        }
        Expr::Function(_, args) => args
            .iter()
            .all(|a| collect_column_refs(a, owner, table, needed)),
        Expr::Aggregate(_, Some(a), _) => collect_column_refs(a, owner, table, needed),
        Expr::Aggregate(_, None, _) => true,
        Expr::InSubquery { .. } | Expr::Exists { .. } | Expr::ScalarSubquery(_) => false,
    }
}

pub(crate) fn items_have_aggregate(items: &[SelectItem]) -> bool {
    items.iter().any(|item| match item {
        SelectItem::Expr(e) | SelectItem::Aliased(e, _) => expr_has_aggregate(e),
        SelectItem::Star => false,
    })
}

fn limit_bound(expr: Option<&Expr>) -> Result<u64> {
    let Some(expr) = expr else {
        return Ok(0);
    };
    match eval_const(expr)? {
        Value::Int(n) if n >= 0 => Ok(n as u64),
        _ => Err(Error::Runtime("limit must be a non-negative integer".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDesc, Schema};

    fn schema(owner: &str, name: &str, dtype: DataType) -> Schema {
        Schema {
            columns: vec![ColumnDesc::plain(Some(owner.to_string()), name.to_string(), dtype)],
        }
    }

    fn eq(left_owner: &str, right_owner: &str) -> Expr {
        Expr::Binary(
            BinOp::Eq,
            Box::new(Expr::QualifiedColumn(left_owner.into(), "id".into())),
            Box::new(Expr::QualifiedColumn(right_owner.into(), "id".into())),
        )
    }

    #[test]
    fn extracts_equi_join_keys() {
        let left = schema("a", "id", DataType::Int);
        let right = schema("b", "id", DataType::Int);
        let keys = analyze_hash_join(JoinKind::Inner, Some(&eq("a", "b")), &left, &right).unwrap();
        assert_eq!(keys.left_keys.len(), 1);
        assert_eq!(keys.right_keys.len(), 1);
        assert!(keys.residual.is_none());
    }

    #[test]
    fn rejects_mismatched_numeric_key_types() {
        let left = schema("a", "id", DataType::Int);
        let right = schema("b", "id", DataType::Float);
        assert!(analyze_hash_join(JoinKind::Inner, Some(&eq("a", "b")), &left, &right).is_none());
    }

    #[test]
    fn cross_join_is_not_hashable() {
        let left = schema("a", "id", DataType::Int);
        let right = schema("b", "id", DataType::Int);
        assert!(analyze_hash_join(JoinKind::Cross, Some(&eq("a", "b")), &left, &right).is_none());
    }

    #[test]
    fn non_equi_predicate_is_not_hashable() {
        let left = schema("a", "id", DataType::Int);
        let right = schema("b", "id", DataType::Int);
        let condition = Expr::Binary(
            BinOp::Gt,
            Box::new(Expr::QualifiedColumn("a".into(), "id".into())),
            Box::new(Expr::QualifiedColumn("b".into(), "id".into())),
        );
        assert!(
            analyze_hash_join(JoinKind::Inner, Some(&condition), &left, &right).is_none()
        );
    }
}
