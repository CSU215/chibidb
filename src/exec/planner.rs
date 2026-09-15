//! The planner is the single place that composes the stages for one statement:
//! fold expressions on the AST, translate a SELECT into a [`LogicalOperator`],
//! optimize the logical plan, then lower it to physical operators.
//!
//! Keeping the composition here means `operator` only ever turns a logical plan
//! into physical operators; it never orchestrates the logical layer itself.

use crate::sql::ast::{Expr, SelectItem, SelectStmt, Stmt};
use crate::{Database, Result};

use super::logical;
use super::operator::{self, ConstantScan, PhysicalOperator, Project, Union};

/// Plans `stmt`, or `None` for statements the operator layer does not cover.
pub fn plan_statement(
    db: &Database,
    stmt: &Stmt,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    // Expression rewrites (constant folding, boolean simplification) run first;
    // they are shared by SELECT and DML.
    let folded = super::optimize::fold_stmt(stmt);
    match &folded {
        Stmt::Select(select) => plan_select(db, select),
        Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) => operator::build_dml(&folded),
        _ => Ok(None),
    }
}

/// Plans a SELECT: translate to a logical plan, optimize it, then lower.
pub fn plan_select(
    db: &Database,
    select: &SelectStmt,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    if !select.set_ops.is_empty() {
        return plan_union(db, select);
    }
    if select.from.is_empty() {
        // A constant SELECT: one projected tuple, no scan.
        if select.items.iter().any(|it| matches!(it, SelectItem::Star))
            || operator::items_have_aggregate(&select.items)
        {
            return Ok(None);
        }
        let (exprs, headers) = projection_exprs(&select.items);
        return Ok(Some(Box::new(Project::new(
            Box::new(ConstantScan::new()),
            exprs,
            headers,
        ))));
    }
    let Some(logical) = logical::translate(select) else {
        return Ok(None);
    };
    let Some(logical) = logical::optimize(db, logical)? else {
        return Ok(None);
    };
    operator::lower(db, select, &logical)
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
