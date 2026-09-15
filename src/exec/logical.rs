//! A relational IR for the FROM / WHERE / JOIN region of a SELECT.
//!
//! Translation ([`logical_from`]) is a pure structural step: it exposes the
//! access path as explicit `Filter`/`Join`/`Scan` nodes instead of the quirks of
//! [`SelectStmt`] (selection on the select, comma joins mixed with `ON`,
//! `set_ops`). Rewrites such as [`pushdown`] then operate on this algebra, and
//! the physical layer lowers the result. Access-path choice (index vs scan,
//! hash vs nested loop) is deliberately *not* here: it stays in lowering.

use crate::catalog::Schema;
use crate::sql::ast::{Expr, JoinKind, SelectStmt, TableRef};
use crate::{Database, Result};

use super::eval::{expr_has_column, expr_has_subquery};
use super::operator::{build_from_source, combine_and, join_clauses, split_conjuncts};

/// One node of the logical plan for a single SELECT's FROM region.
pub(crate) enum LogicalOperator {
    /// A base table or view, with its alias resolved by the caller.
    Scan(TableRef),
    Filter { input: Box<LogicalOperator>, predicate: Expr },
    Join {
        left: Box<LogicalOperator>,
        right: Box<LogicalOperator>,
        kind: JoinKind,
        on: Option<Expr>,
    },
}

/// Translates a SELECT's FROM / WHERE / JOIN region into a logical plan. The
/// WHERE clause becomes one `Filter` above the join tree; it is not yet pushed.
/// Returns `None` for a SELECT without FROM.
pub(crate) fn logical_from(select: &SelectStmt) -> Option<LogicalOperator> {
    let first = select.from.first()?;
    let mut node = LogicalOperator::Scan(first.clone());
    for (i, (kind, on)) in join_clauses(select).into_iter().enumerate() {
        let right = LogicalOperator::Scan(select.from[i + 1].clone());
        node = LogicalOperator::Join {
            left: Box::new(node),
            right: Box::new(right),
            kind,
            on,
        };
    }
    if let Some(selection) = &select.selection {
        node = LogicalOperator::Filter {
            input: Box::new(node),
            predicate: selection.clone(),
        };
    }
    Some(node)
}

/// The output schema of a logical node, `None` when a source cannot be built
/// (the caller then falls back to the materialized executor).
fn schema(db: &Database, node: &LogicalOperator) -> Result<Option<Schema>> {
    match node {
        LogicalOperator::Scan(tref) => {
            Ok(build_from_source(db, tref)?.map(|op| op.schema().clone()))
        }
        LogicalOperator::Filter { input, .. } => schema(db, input),
        LogicalOperator::Join { left, right, .. } => {
            let (Some(mut left), Some(right)) = (schema(db, left)?, schema(db, right)?) else {
                return Ok(None);
            };
            left.columns.extend(right.columns);
            Ok(Some(left))
        }
    }
}

/// Pushes WHERE conjuncts that reference a single source onto that source.
/// Only inner/comma join chains are eligible: below an outer join the
/// null-extension would change which rows are produced.
pub(crate) fn pushdown(
    db: &Database,
    node: LogicalOperator,
) -> Result<Option<LogicalOperator>> {
    if !inner_only(&node) {
        return Ok(Some(node));
    }
    let (input, selection) = match node {
        LogicalOperator::Filter { input, predicate } => (*input, Some(predicate)),
        other => (other, None),
    };
    let Some(selection) = selection else {
        return Ok(Some(input));
    };
    let conjuncts: Vec<Expr> = split_conjuncts(&selection).into_iter().cloned().collect();
    let mut scans = Vec::new();
    collect_scans(&input, &mut scans);
    let mut schemas = Vec::with_capacity(scans.len());
    for scan in &scans {
        let Some(schema) = schema(db, scan)? else {
            return Ok(None);
        };
        schemas.push(schema);
    }
    let mut pushed: Vec<Vec<Expr>> = vec![Vec::new(); schemas.len()];
    let mut kept = Vec::new();
    for conjunct in conjuncts {
        match sole_source(&conjunct, &schemas) {
            Some(i) => pushed[i].push(conjunct),
            None => kept.push(conjunct),
        }
    }
    let mut index = 0;
    let rebuilt = rebuild(input, &pushed, &mut index);
    Ok(Some(match combine_and(kept) {
        Some(predicate) => LogicalOperator::Filter { input: Box::new(rebuilt), predicate },
        None => rebuilt,
    }))
}

fn inner_only(node: &LogicalOperator) -> bool {
    match node {
        LogicalOperator::Scan(_) => true,
        LogicalOperator::Filter { input, .. } => inner_only(input),
        LogicalOperator::Join { left, right, kind, .. } => {
            matches!(kind, JoinKind::Inner | JoinKind::Cross)
                && inner_only(left)
                && inner_only(right)
        }
    }
}

fn collect_scans<'a>(node: &'a LogicalOperator, out: &mut Vec<&'a LogicalOperator>) {
    match node {
        LogicalOperator::Scan(_) => out.push(node),
        LogicalOperator::Filter { input, .. } => collect_scans(input, out),
        LogicalOperator::Join { left, right, .. } => {
            collect_scans(left, out);
            collect_scans(right, out);
        }
    }
}

/// Rebuilds the tree in the same left-to-right order [`collect_scans`] used,
/// wrapping each scan in the predicates assigned to it.
fn rebuild(node: LogicalOperator, pushed: &[Vec<Expr>], index: &mut usize) -> LogicalOperator {
    match node {
        LogicalOperator::Scan(tref) => {
            let i = *index;
            *index += 1;
            let scan = LogicalOperator::Scan(tref);
            match combine_and(pushed[i].clone()) {
                Some(predicate) => {
                    LogicalOperator::Filter { input: Box::new(scan), predicate }
                }
                None => scan,
            }
        }
        LogicalOperator::Filter { input, predicate } => LogicalOperator::Filter {
            input: Box::new(rebuild(*input, pushed, index)),
            predicate,
        },
        LogicalOperator::Join { left, right, kind, on } => LogicalOperator::Join {
            left: Box::new(rebuild(*left, pushed, index)),
            right: Box::new(rebuild(*right, pushed, index)),
            kind,
            on,
        },
    }
}

/// The single source that owns every column of `expr`, or `None` when the
/// expression has no column, carries a subquery, or resolves in more than one
/// source (ambiguous, so pushing it would hide the ambiguity).
fn sole_source(expr: &Expr, sources: &[Schema]) -> Option<usize> {
    if !expr_has_column(expr) || expr_has_subquery(expr) {
        return None;
    }
    let mut found = None;
    for (i, schema) in sources.iter().enumerate() {
        if expr_resolves(expr, schema) {
            if found.is_some() {
                return None;
            }
            found = Some(i);
        }
    }
    found
}

/// Whether every column reference in `expr` resolves against `schema`.
fn expr_resolves(expr: &Expr, schema: &Schema) -> bool {
    match expr {
        Expr::Column(name) => schema.index_of(name).is_some(),
        Expr::QualifiedColumn(owner, name) => schema
            .columns
            .iter()
            .any(|c| c.owner.as_deref() == Some(owner) && &c.name == name),
        Expr::Unary(_, e) => expr_resolves(e, schema),
        Expr::Binary(_, l, r) => expr_resolves(l, schema) && expr_resolves(r, schema),
        Expr::IsNull(e, _) => expr_resolves(e, schema),
        Expr::Like { expr, pattern, .. } => {
            expr_resolves(expr, schema) && expr_resolves(pattern, schema)
        }
        Expr::Function(_, args) => args.iter().all(|a| expr_resolves(a, schema)),
        // Literals, values and anything else without a column resolve trivially.
        _ => true,
    }
}
