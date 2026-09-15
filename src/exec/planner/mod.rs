//! The planner is the single place that composes the stages for one statement:
//! fold expressions on the AST, translate a SELECT into a [`logical::LogicalOperator`],
//! optimize the logical plan, then lower it to physical operators.
//!
//! Keeping the composition here means `operator` only ever turns a logical plan
//! into physical operators; it never orchestrates the logical layer itself.
//!
//! [`plan_statement_layers`] hands back the intermediate logical plan alongside
//! the physical one, so EXPLAIN and the admin API do not have to rebuild it.

mod access;
mod explain;
mod fold;
pub(crate) mod logical;
pub(crate) mod lower;
mod util;

pub(crate) use access::{ordered_index_file, plan_index_scan};
pub(crate) use explain::execute_explain;

use std::cell::RefCell;
use std::rc::Rc;

use crate::sql::ast::{Expr, SelectItem, SelectStmt, Stmt};
use crate::{Database, Result};

use super::operator::{PhysicalOperator, PlannedSubqueries, SubqueryRegistry};
use logical::LogicalOperator;

/// The result of planning: the optimized logical plan plus the physical plan
/// that executes it. Both are `None` for statements the operator layer does not
/// cover (DDL, SHOW, transaction control).
pub(crate) struct Layers {
    pub(crate) logical: Option<LogicalOperator>,
    pub(crate) physical: Option<Box<dyn PhysicalOperator>>,
}

/// Convenience over [`plan_statement_layers`] that returns only the executable
/// plan. It still runs the whole pipeline; use the layered form when the
/// intermediate [`LogicalOperator`] is needed.
pub fn plan_statement(
    db: &Database,
    stmt: &Stmt,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    Ok(plan_statement_layers(db, stmt)?.physical)
}

/// Plans `stmt` and returns every stage, sharing the logical plan instead of
/// rebuilding it. Every operator statement goes through the same pipeline:
/// translate to a logical plan, optimize it, then lower it.
pub(crate) fn plan_statement_layers(db: &Database, stmt: &Stmt) -> Result<Layers> {
    // Expression rewrites (constant folding, boolean simplification) run first;
    // they are shared by SELECT and DML.
    let folded = fold::fold_stmt(stmt);
    let Some(logical) = logical::translate_stmt(db, &folded)? else {
        return Ok(Layers { logical: None, physical: None });
    };
    let Some(logical) = logical::optimize(db, logical)? else {
        return Ok(Layers { logical: None, physical: None });
    };
    let physical = lower::lower(db, &logical)?;
    let physical = attach_subqueries(db, &logical, physical)?;
    Ok(Layers { logical: Some(logical), physical })
}

/// Lowers every subquery the plan's expressions can reach and wraps the
/// physical plan so the evaluator runs stored plans instead of planning while a
/// row is being evaluated.
fn attach_subqueries(
    db: &Database,
    logical: &LogicalOperator,
    physical: Option<Box<dyn PhysicalOperator>>,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    let Some(physical) = physical else {
        return Ok(None);
    };
    let mut registry = SubqueryRegistry::new();
    collect_logical_subqueries(db, logical, &mut registry)?;
    if registry.is_empty() {
        return Ok(Some(physical));
    }
    Ok(Some(Box::new(PlannedSubqueries::new(physical, Rc::new(registry)))))
}

/// Walks a logical plan's expressions, lowering any subquery they contain.
fn collect_logical_subqueries(
    db: &Database,
    node: &LogicalOperator,
    registry: &mut SubqueryRegistry,
) -> Result<()> {
    match node {
        LogicalOperator::Scan(_) | LogicalOperator::Constant => {}
        LogicalOperator::Filter { input, predicate } => {
            collect_expr_subqueries(db, predicate, registry)?;
            collect_logical_subqueries(db, input, registry)?;
        }
        LogicalOperator::Join { left, right, on, .. } => {
            if let Some(on) = on {
                collect_expr_subqueries(db, on, registry)?;
            }
            collect_logical_subqueries(db, left, registry)?;
            collect_logical_subqueries(db, right, registry)?;
        }
        LogicalOperator::Aggregate { input, group_by, aggregates } => {
            for e in group_by {
                collect_expr_subqueries(db, e, registry)?;
            }
            for e in aggregates {
                collect_expr_subqueries(db, e, registry)?;
            }
            collect_logical_subqueries(db, input, registry)?;
        }
        LogicalOperator::Having { input, predicate } => {
            collect_expr_subqueries(db, predicate, registry)?;
            collect_logical_subqueries(db, input, registry)?;
        }
        LogicalOperator::Project { input, items } => {
            for item in items {
                collect_item_subqueries(db, item, registry)?;
            }
            collect_logical_subqueries(db, input, registry)?;
        }
        LogicalOperator::Sort { input, order_by } => {
            for (e, _) in order_by {
                collect_expr_subqueries(db, e, registry)?;
            }
            collect_logical_subqueries(db, input, registry)?;
        }
        LogicalOperator::Distinct { input } => {
            collect_logical_subqueries(db, input, registry)?;
        }
        LogicalOperator::Limit { input, limit } => {
            collect_expr_subqueries(db, &limit.count, registry)?;
            if let Some(offset) = &limit.offset {
                collect_expr_subqueries(db, offset, registry)?;
            }
            collect_logical_subqueries(db, input, registry)?;
        }
        LogicalOperator::Union { inputs, order_by, limit } => {
            for (e, _) in order_by {
                collect_expr_subqueries(db, e, registry)?;
            }
            if let Some(limit) = limit {
                collect_expr_subqueries(db, &limit.count, registry)?;
                if let Some(offset) = &limit.offset {
                    collect_expr_subqueries(db, offset, registry)?;
                }
            }
            for (_, input) in inputs {
                collect_logical_subqueries(db, input, registry)?;
            }
        }
        LogicalOperator::Insert(cmd) => {
            for row in &cmd.rows {
                for e in row {
                    collect_expr_subqueries(db, e, registry)?;
                }
            }
        }
        LogicalOperator::Update(cmd) => {
            for (_, e) in &cmd.assignments {
                collect_expr_subqueries(db, e, registry)?;
            }
            if let Some(selection) = &cmd.selection {
                collect_expr_subqueries(db, selection, registry)?;
            }
        }
        LogicalOperator::Delete(cmd) => {
            if let Some(selection) = &cmd.selection {
                collect_expr_subqueries(db, selection, registry)?;
            }
        }
    }
    Ok(())
}

fn collect_item_subqueries(
    db: &Database,
    item: &SelectItem,
    registry: &mut SubqueryRegistry,
) -> Result<()> {
    match item {
        SelectItem::Star => Ok(()),
        SelectItem::Expr(e) | SelectItem::Aliased(e, _) => {
            collect_expr_subqueries(db, e, registry)
        }
    }
}

fn collect_expr_subqueries(
    db: &Database,
    expr: &Expr,
    registry: &mut SubqueryRegistry,
) -> Result<()> {
    match expr {
        Expr::InSubquery { expr, sub, .. } => {
            collect_expr_subqueries(db, expr, registry)?;
            lower_subquery(db, sub, registry)?;
        }
        Expr::Exists { sub } | Expr::ScalarSubquery(sub) => {
            lower_subquery(db, sub, registry)?;
        }
        Expr::Unary(_, a) => collect_expr_subqueries(db, a, registry)?,
        Expr::Binary(_, l, r) => {
            collect_expr_subqueries(db, l, registry)?;
            collect_expr_subqueries(db, r, registry)?;
        }
        Expr::IsNull(a, _) => collect_expr_subqueries(db, a, registry)?,
        Expr::Like { expr, pattern, .. } => {
            collect_expr_subqueries(db, expr, registry)?;
            collect_expr_subqueries(db, pattern, registry)?;
        }
        Expr::Function(_, args) => {
            for a in args {
                collect_expr_subqueries(db, a, registry)?;
            }
        }
        Expr::Aggregate(_, Some(a), _) => collect_expr_subqueries(db, a, registry)?,
        _ => {}
    }
    Ok(())
}

fn lower_subquery(
    db: &Database,
    sub: &SelectStmt,
    registry: &mut SubqueryRegistry,
) -> Result<()> {
    let key = format!("{sub:?}");
    if registry.contains_key(&key) {
        return Ok(());
    }
    // Recurse through `plan_select` so a subquery's own subqueries are lowered
    // and hosted by its plan as well.
    let plan = plan_select(db, sub)?;
    registry.insert(key, RefCell::new(plan));
    Ok(())
}

/// Plans a SELECT: translate to a logical plan, optimize it, then lower.
pub fn plan_select(
    db: &Database,
    select: &SelectStmt,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    Ok(plan_statement_layers(db, &Stmt::Select(Box::new(select.clone())))?.physical)
}
