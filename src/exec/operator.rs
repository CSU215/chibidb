use crate::ast::{DataType, Expr, SelectItem, SelectStmt};
use crate::catalog::{ColumnDesc, Schema};
use crate::storage::codec::decode_record;
use crate::storage::engine::{HeapEngine, RowScanner, TableEngine};
use crate::storage::heap::HeapFile;
use crate::storage::Rid;
use crate::trx::Session;
use crate::value::Value;
use crate::{Database, Error, Result};

use super::aggregate::expr_has_aggregate;
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
        let columns = db.catalog().table(table)?.schema.columns.clone();
        let owner = table.to_string();
        let schema = Schema {
            columns: columns
                .into_iter()
                .map(|c| ColumnDesc::plain(Some(owner.clone()), c.name, c.dtype))
                .collect(),
        };
        Ok(Self { table: owner, schema, scanner: None })
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

/// Index scan: fetches exactly the row ids the access path selected.
pub struct IndexScan {
    schema: Schema,
    heap_file: crate::storage::FileId,
    rids: Vec<Rid>,
    pos: usize,
}

impl IndexScan {
    /// Returns `None` when the selection is not sargable (use a `TableScan`).
    pub fn new(db: &mut Database, table: &str, selection: Option<&Expr>) -> Result<Option<Self>> {
        let Some(plan) = crate::exec::plan::plan_index_scan(db, table, selection)? else {
            return Ok(None);
        };
        let columns = db.catalog().table(table)?.schema.columns.clone();
        let owner = table.to_string();
        let schema = Schema {
            columns: columns
                .into_iter()
                .map(|c| ColumnDesc::plain(Some(owner.clone()), c.name, c.dtype))
                .collect(),
        };
        Ok(Some(Self { schema, heap_file: plan.heap_file, rids: plan.rids, pos: 0 }))
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

/// Builds a physical plan for a single-table SELECT covering selection,
/// projection and limit. Returns `None` for shapes the operators do not
/// handle yet (joins, aggregates, ordering, distinct, set ops, views).
pub fn build_simple_select(
    db: &mut Database,
    select: &SelectStmt,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    if !simple_select_shape(select) {
        return Ok(None);
    }
    let table = &select.from[0].name;
    if db.catalog().view(table).is_some() {
        return Ok(None);
    }
    let mut op: Box<dyn PhysicalOperator> =
        match IndexScan::new(db, table, select.selection.as_ref())? {
            Some(scan) => Box::new(scan),
            None => Box::new(TableScan::new(db, table)?),
        };
    if let Some(selection) = &select.selection {
        op = Box::new(Filter::new(op, selection.clone()));
    }
    let (exprs, headers) = projection(db, table, &select.items)?;
    op = Box::new(Project::new(op, exprs, headers));
    if let Some(limit) = &select.limit {
        let offset = limit_bound(limit.offset.as_ref())?;
        let count = limit_bound(Some(&limit.count))?;
        op = Box::new(Limit::new(op, offset, Some(count)));
    }
    Ok(Some(op))
}

fn simple_select_shape(select: &SelectStmt) -> bool {
    if !select.set_ops.is_empty()
        || select.from.len() != 1
        || !select.group_by.is_empty()
        || select.having.is_some()
        || select.distinct
        || !select.order_by.is_empty()
    {
        return false;
    }
    !select.items.iter().any(|item| match item {
        SelectItem::Expr(e) | SelectItem::Aliased(e, _) => expr_has_aggregate(e),
        SelectItem::Star => false,
    })
}

fn projection(db: &Database, table: &str, items: &[SelectItem]) -> Result<(Vec<Expr>, Vec<String>)> {
    let mut exprs = Vec::new();
    let mut headers = Vec::new();
    for item in items {
        match item {
            SelectItem::Star => {
                for column in &db.catalog().table(table)?.schema.columns {
                    headers.push(column.name.clone());
                    exprs.push(Expr::Column(column.name.clone()));
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
    Ok((exprs, headers))
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
