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

pub(crate) use access::{ordered_index_file, plan_index_scan, resolved_order_column};
pub(crate) use explain::execute_explain;

use crate::sql::ast::{Expr, SelectItem, SelectStmt, Stmt};
use crate::{Database, Result};

use super::operator::{self, ConstantScan, PhysicalOperator, Project, Union};
use logical::LogicalOperator;
use lower::items_have_aggregate;

/// The result of planning: the optimized logical plan (when the shape has a
/// single one) plus the physical plan that executes it. `logical` is `None` for
/// DML, a constant SELECT or a UNION chain.
pub(crate) struct Layers {
    pub(crate) logical: Option<LogicalOperator>,
    pub(crate) physical: Option<Box<dyn PhysicalOperator>>,
}

/// Plans `stmt`, or `None` for statements the operator layer does not cover.
pub fn plan_statement(
    db: &Database,
    stmt: &Stmt,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    Ok(plan_statement_layers(db, stmt)?.physical)
}

/// Plans `stmt` and returns every stage, sharing the logical plan instead of
/// rebuilding it.
pub(crate) fn plan_statement_layers(db: &Database, stmt: &Stmt) -> Result<Layers> {
    // Expression rewrites (constant folding, boolean simplification) run first;
    // they are shared by SELECT and DML.
    let folded = fold::fold_stmt(stmt);
    match &folded {
        Stmt::Select(select) => plan_select_layers(db, select),
        Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) => Ok(Layers {
            logical: None,
            physical: operator::build_dml(&folded)?,
        }),
        _ => Ok(Layers { logical: None, physical: None }),
    }
}

/// Plans a SELECT: translate to a logical plan, optimize it, then lower.
pub fn plan_select(
    db: &Database,
    select: &SelectStmt,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    Ok(plan_select_layers(db, select)?.physical)
}

fn plan_select_layers(db: &Database, select: &SelectStmt) -> Result<Layers> {
    if !select.set_ops.is_empty() {
        return Ok(Layers { logical: None, physical: plan_union(db, select)? });
    }
    if select.from.is_empty() {
        // A constant SELECT: one projected tuple, no scan.
        if select.items.iter().any(|it| matches!(it, SelectItem::Star))
            || items_have_aggregate(&select.items)
        {
            return Ok(Layers { logical: None, physical: None });
        }
        let (exprs, headers) = projection_exprs(&select.items);
        let physical = Box::new(Project::new(
            Box::new(ConstantScan::new()),
            exprs,
            headers,
        ));
        return Ok(Layers { logical: None, physical: Some(physical) });
    }
    let Some(logical) = logical::translate(select) else {
        return Ok(Layers { logical: None, physical: None });
    };
    let Some(logical) = logical::optimize(db, logical)? else {
        return Ok(Layers { logical: None, physical: None });
    };
    let physical = lower::lower(db, select, &logical)?;
    Ok(Layers { logical: Some(logical), physical })
}

/// Plans a UNION chain: each operand is planned, then the trailing ORDER BY /
/// LIMIT apply to the whole result.
fn plan_union(
    db: &Database,
    select: &SelectStmt,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    let mut base = select.clone();
    base.set_ops = Vec::new();
    let order_by = std::mem::take(&mut base.order_by);
    let limit = base.limit.take();
    let Some(base_plan) = plan_select(db, &base)? else {
        return Ok(None);
    };
    let mut inputs: Vec<(bool, Box<dyn PhysicalOperator>)> = vec![(true, base_plan)];
    for (all, operand) in &select.set_ops {
        let Some(plan) = plan_select(db, operand)? else {
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
