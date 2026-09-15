use crate::catalog::{ColumnDesc, Schema};
use crate::sql::ast::{BinOp, Expr, Limit as LimitClause, SelectItem};
use crate::value::{DataType, Value};
use crate::{Error, Result};

use super::join::drain;
use super::{ExecContext, PhysicalOperator};
use crate::exec::aggregate::{self, sort_rows};
use crate::exec::chunk::{Chunk, Column};
use crate::exec::eval::{eval_binary, eval_const, EvalCtx};
use crate::exec::sort_projected;
use crate::exec::subquery::{eval_bound, eval_predicate_bound};

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
    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn label(&self) -> String {
        format!("Filter {}", self.predicate)
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
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
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn label(&self) -> String {
        format!("Project cols={}", self.exprs.len())
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
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
    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn label(&self) -> String {
        format!("Limit offset={} count={:?}", self.offset, self.count)
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
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
    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn label(&self) -> String {
        "Distinct".to_string()
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
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
    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn label(&self) -> String {
        format!("Sort keys={}", self.order_by.len())
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
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
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn label(&self) -> String {
        format!("Union arms={}", self.inputs.len())
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        self.inputs.iter().map(|(_, op)| op.as_ref()).collect()
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
                aggregate::dedup_rows(&mut rows);
            }
        }
        if !self.order_by.is_empty() {
            sort_projected(
                ctx.db,
                ctx.trx,
                ctx.outer,
                &columns,
                &mut rows,
                &self.order_by,
            )?;
        }
        aggregate::apply_limit(&mut rows, &self.limit)?;
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
