use crate::ast::{DataType, Expr, JoinKind, Limit as LimitClause, SelectItem, SelectStmt};
use crate::catalog::{ColumnDesc, Schema};
use crate::storage::codec::decode_record;
use crate::storage::engine::{HeapEngine, RowScanner, TableEngine};
use crate::storage::heap::HeapFile;
use crate::storage::Rid;
use crate::trx::Session;
use crate::value::Value;
use crate::{Database, Error, Result};

use super::aggregate::{expr_has_aggregate, sort_rows};
use super::eval::{eval_const, EvalCtx};
use super::subquery::{eval_bound, eval_predicate_bound};

/// Context threaded through operators: the database plus the session whose
/// active transaction supplies MVCC visibility.
pub struct ExecContext<'a> {
    pub db: &'a mut Database,
    pub session: &'a mut Session,
}

/// Volcano-style physical operator: `open`, repeated `next`, `close`.
pub trait PhysicalOperator {
    fn schema(&self) -> &Schema;
    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()>;
    fn next(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>>;
    fn close(&mut self) -> Result<()>;
}

/// Sequential scan of a table, filtering by MVCC visibility.
pub struct TableScan {
    table: String,
    schema: Schema,
    scanner: Option<Box<dyn RowScanner>>,
}

impl TableScan {
    pub fn new(db: &Database, table: &str) -> Result<Self> {
        Self::with_owner(db, table, table)
    }

    /// `owner` is the alias (or table name) that qualifies this scan's
    /// columns, so qualified references resolve correctly.
    pub fn with_owner(db: &Database, table: &str, owner: &str) -> Result<Self> {
        let columns = db.catalog().table(table)?.schema.columns.clone();
        let owner = owner.to_string();
        let schema = Schema {
            columns: columns
                .into_iter()
                .map(|c| ColumnDesc::plain(Some(owner.clone()), c.name, c.dtype))
                .collect(),
        };
        Ok(Self { table: table.to_string(), schema, scanner: None })
    }
}

impl PhysicalOperator for TableScan {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        let file = ctx.db.catalog().table(&self.table)?.heap.file;
        self.scanner = Some(HeapEngine::new(file).scan(&mut ctx.db.pool)?);
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        loop {
            let (creator, deleter, row) = {
                let scanner = self.scanner.as_mut().expect("table scan not opened");
                match scanner.next(&mut ctx.db.pool)? {
                    Some((_, record)) => decode_record(&record)?,
                    None => return Ok(None),
                }
            };
            if ctx.session.trx().visible(creator, deleter) {
                return Ok(Some(row));
            }
        }
    }

    fn close(&mut self) -> Result<()> {
        self.scanner = None;
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
                ctx.session.trx(),
                &self.predicate,
                self.child.schema(),
                &row,
                None,
            )? {
                return Ok(Some(row));
            }
        }
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

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.child.open(ctx)
    }

    fn next(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        let Some(row) = self.child.next(ctx)? else {
            return Ok(None);
        };
        let eval_ctx = EvalCtx::row(self.child.schema(), &row);
        let mut out = Vec::with_capacity(self.exprs.len());
        for expr in &self.exprs {
            out.push(eval_bound(ctx.db, ctx.session.trx(), expr, Some(&eval_ctx))?);
        }
        Ok(Some(out))
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
            ctx.session.trx(),
            None,
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
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.child.open(ctx)?;
        let mut filtered = Vec::new();
        while let Some(row) = self.child.next(ctx)? {
            filtered.push(row);
        }
        self.child.close()?;
        let schema = self.child.schema().clone();
        let rows = super::aggregate::grouped_select_rows(
            ctx.db,
            ctx.session.trx(),
            None,
            &schema,
            &self.select,
            filtered,
            self.exprs.clone(),
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
                eval_predicate_bound(ctx.db, ctx.session.trx(), condition, &self.schema, row, None)
            }
        }
    }
}

impl PhysicalOperator for NestedLoopJoin {
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
                ctx.session.trx(),
                None,
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
    schema: Schema,
    heap_file: crate::storage::FileId,
    column: String,
    rids: Vec<Rid>,
    pos: usize,
}

impl IndexScan {
    /// Returns `None` when the selection is not sargable (use a `TableScan`).
    pub fn new(db: &mut Database, table: &str, selection: Option<&Expr>) -> Result<Option<Self>> {
        Self::with_owner(db, table, table, selection)
    }

    /// `owner` is the alias (or table name) that qualifies this scan's columns.
    pub fn with_owner(
        db: &mut Database,
        table: &str,
        owner: &str,
        selection: Option<&Expr>,
    ) -> Result<Option<Self>> {
        let Some(plan) = crate::exec::plan::plan_index_scan(db, table, selection)? else {
            return Ok(None);
        };
        let columns = db.catalog().table(table)?.schema.columns.clone();
        let owner = owner.to_string();
        let schema = Schema {
            columns: columns
                .into_iter()
                .map(|c| ColumnDesc::plain(Some(owner.clone()), c.name, c.dtype))
                .collect(),
        };
        Ok(Some(Self {
            schema,
            heap_file: plan.heap_file,
            column: plan.column,
            rids: plan.rids,
            pos: 0,
        }))
    }

    /// The indexed column, which the scan yields in ascending order.
    pub fn ordered_column(&self) -> &str {
        &self.column
    }
}

impl PhysicalOperator for IndexScan {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, _ctx: &mut ExecContext<'_>) -> Result<()> {
        self.pos = 0;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        while self.pos < self.rids.len() {
            let rid = self.rids[self.pos];
            self.pos += 1;
            let record = HeapFile::at(self.heap_file).get(&mut ctx.db.pool, rid)?;
            let (creator, deleter, row) = decode_record(&record)?;
            if ctx.session.trx().visible(creator, deleter) {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    fn close(&mut self) -> Result<()> {
        self.pos = self.rids.len();
        Ok(())
    }
}

/// Builds a physical plan for a SELECT. Returns `None` for any shape the
/// operators do not cover yet, leaving the materialized executor as fallback.
pub fn build_select(
    db: &mut Database,
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
    // left-deep nested-loop join over plain scans.
    let (mut op, ordered_by): (Box<dyn PhysicalOperator>, Option<String>) = if select.from.len() == 1
    {
        let table = &select.from[0].name;
        if db.catalog().view(table).is_some() {
            return Ok(None);
        }
        let owner = select.from[0].alias.as_deref().unwrap_or(table);
        match IndexScan::with_owner(db, table, owner, select.selection.as_ref())? {
            Some(scan) => {
                let column = scan.ordered_column().to_string();
                (Box::new(scan), Some(column))
            }
            None => (Box::new(TableScan::with_owner(db, table, owner)?), None),
        }
    } else {
        let first = &select.from[0];
        if db.catalog().view(&first.name).is_some() {
            return Ok(None);
        }
        let owner = first.alias.as_deref().unwrap_or(&first.name);
        let mut op: Box<dyn PhysicalOperator> =
            Box::new(TableScan::with_owner(db, &first.name, owner)?);
        for i in 1..select.from.len() {
            let tref = &select.from[i];
            if db.catalog().view(&tref.name).is_some() {
                return Ok(None);
            }
            let owner = tref.alias.as_deref().unwrap_or(&tref.name);
            let right: Box<dyn PhysicalOperator> =
                Box::new(TableScan::with_owner(db, &tref.name, owner)?);
            let kind = select.joins.get(i).copied().unwrap_or(JoinKind::Cross);
            let condition = select.on.get(i - 1).cloned();
            op = Box::new(NestedLoopJoin::new(op, right, kind, condition)?);
        }
        (op, None)
    };
    if let Some(selection) = &select.selection {
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
        let skip = ordered_by
            .as_deref()
            .is_some_and(|column| crate::exec::plan::order_by_matches(column, &select.order_by));
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
    db: &mut Database,
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

fn items_have_aggregate(items: &[SelectItem]) -> bool {
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
