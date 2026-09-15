use crate::catalog::Schema;
use crate::sql::ast::{Expr, Stmt};
use crate::value::Value;
use crate::{Database, Result};

use super::chunk::Chunk;
use super::eval::EvalCtx;

mod basic;
mod index_scan;
mod join;
mod scan;

pub use basic::{Distinct, Filter, Limit, Project, Sort, Union};
pub use index_scan::IndexScan;
pub use join::{HashJoin, NestedLoopJoin};
pub use scan::{ConstantScan, TableScan, ViewScan};
/// Context threaded through operators: the database, the session's active
/// transaction, and the outer row/group context when this plan runs as a
/// correlated subquery.
pub struct ExecContext<'a> {
    pub(crate) db: &'a Database,
    pub(crate) trx: &'a mut crate::txn::trx::TrxState,
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

    /// When this operator is (or wraps) an aggregate, its input schema and the
    /// aggregate expressions whose results are the trailing `#aggN` columns.
    /// Lets projection/HAVING/ORDER BY above it rewrite aggregates to columns.
    fn aggregate_parts(&self) -> Option<(&Schema, &[Expr])> {
        None
    }

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

    /// A short one-line label for this node (for EXPLAIN / visualisation).
    fn label(&self) -> String {
        "Operator".to_string()
    }

    /// Child operators in execution order (empty for leaves).
    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        Vec::new()
    }
}

/// Renders an indented tree of `plan`, two spaces per depth, one line per
/// operator, newline-terminated.
pub fn physical_tree(plan: &dyn PhysicalOperator) -> String {
    fn walk(op: &dyn PhysicalOperator, depth: usize, out: &mut String) {
        for _ in 0..depth {
            out.push_str("  ");
        }
        out.push_str(&op.label());
        out.push('\n');
        for child in op.children() {
            walk(child, depth + 1, out);
        }
    }
    let mut out = String::new();
    walk(plan, 0, &mut out);
    out
}

/// Builds the physical command for a DML statement; `None` for anything else.
/// SELECT planning lives in [`super::planner`].
pub(crate) fn build_dml(stmt: &Stmt) -> Result<Option<Box<dyn PhysicalOperator>>> {
    Ok(match stmt {
        Stmt::Insert(insert) => {
            Some(Box::new(crate::exec::dml::InsertOp::new(insert.clone())))
        }
        Stmt::Update(update) => {
            Some(Box::new(crate::exec::dml::UpdateOp::new(update.clone())))
        }
        Stmt::Delete(delete) => {
            Some(Box::new(crate::exec::dml::DeleteOp::new(delete.clone())))
        }
        _ => None,
    })
}