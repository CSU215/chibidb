use crate::ast::{DataType, Expr};
use crate::catalog::{ColumnDesc, Schema};
use crate::storage::codec::decode_record;
use crate::storage::engine::{HeapEngine, RowScanner, TableEngine};
use crate::trx::Session;
use crate::value::Value;
use crate::{Database, Result};

use super::eval::EvalCtx;
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
