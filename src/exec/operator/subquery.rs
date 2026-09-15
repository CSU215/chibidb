//! A plan-time subquery registry and the transparent operator that hosts it.
//!
//! Lowering collects every subquery an expression can reach, lowers each to a
//! plan, and wraps the plan in [`PlannedSubqueries`]. While the wrapper runs,
//! the registry is pushed onto a thread-local stack so the expression evaluator
//! can execute a subquery's already-built plan instead of planning at runtime.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::catalog::Schema;
use crate::sql::ast::Expr;
use crate::value::Value;
use crate::Result;

use super::{ExecContext, OutputKind, PhysicalOperator, ProjectedSink};

/// Lowered subquery plans keyed by a canonical rendering of their definition.
/// A subquery may be re-run once per outer row, so each plan must be re-openable.
pub(crate) type SubqueryRegistry = HashMap<String, RefCell<Option<Box<dyn PhysicalOperator>>>>;

thread_local! {
    /// Registries active on this thread, innermost last. The evaluator looks up
    /// the top of the stack, so a subquery hosted by an inner plan wins.
    static ACTIVE: RefCell<Vec<Rc<SubqueryRegistry>>> = const { RefCell::new(Vec::new()) };
}

/// The registry of the innermost running plan, if any.
pub(crate) fn current_registry() -> Option<Rc<SubqueryRegistry>> {
    ACTIVE.with(|s| s.borrow().last().cloned())
}

/// Pushes a registry for as long as it is held; popped on drop.
struct SubqueryScope;

impl SubqueryScope {
    fn enter(registry: Rc<SubqueryRegistry>) -> Self {
        ACTIVE.with(|s| s.borrow_mut().push(registry));
        Self
    }
}

impl Drop for SubqueryScope {
    fn drop(&mut self) {
        ACTIVE.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

/// A physical plan plus the lowered subplans its expressions can evaluate. It is
/// transparent to the operator tree: `label`/`children`/`schema` and every
/// execution method delegate to the child.
pub(crate) struct PlannedSubqueries {
    child: Box<dyn PhysicalOperator>,
    registry: Rc<SubqueryRegistry>,
    scope: Option<SubqueryScope>,
}

impl PlannedSubqueries {
    pub(crate) fn new(child: Box<dyn PhysicalOperator>, registry: Rc<SubqueryRegistry>) -> Self {
        Self { child, registry, scope: None }
    }
}

impl PhysicalOperator for PlannedSubqueries {
    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn aggregate_parts(&self) -> Option<(&Schema, &[Expr])> {
        self.child.aggregate_parts()
    }

    fn label(&self) -> String {
        self.child.label()
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        self.child.children()
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        // Push first: blocking children (Sort, Aggregate, joins) may evaluate
        // expressions, and therefore subqueries, from within their own `open`.
        self.scope = Some(SubqueryScope::enter(self.registry.clone()));
        if let Err(e) = self.child.open(ctx) {
            self.scope = None;
            return Err(e);
        }
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        self.child.next(ctx)
    }

    fn next_chunk(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<crate::exec::chunk::Chunk>> {
        self.child.next_chunk(ctx)
    }

    fn chunk_native(&self) -> bool {
        self.child.chunk_native()
    }

    fn for_each_row(
        &mut self,
        ctx: &mut ExecContext<'_>,
        sink: &mut dyn FnMut(&[Value]) -> Result<()>,
    ) -> Result<bool> {
        self.child.for_each_row(ctx, sink)
    }

    fn for_each_projected_row(
        &mut self,
        ctx: &mut ExecContext<'_>,
        cols: &[usize],
        sink: &mut ProjectedSink<'_>,
    ) -> Result<Option<bool>> {
        self.child.for_each_projected_row(ctx, cols, sink)
    }

    fn close(&mut self) -> Result<()> {
        let result = self.child.close();
        self.scope = None;
        result
    }

    fn output_kind(&self) -> OutputKind {
        self.child.output_kind()
    }

    fn affected_rows(&self) -> Option<u64> {
        self.child.affected_rows()
    }
}
