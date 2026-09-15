//! A relational IR for a SELECT.
//!
//! Translation ([`logical_select`]) is a pure structural step: it exposes the
//! query as explicit `Scan`/`Filter`/`Join`/`Project`/`Aggregate`/`Sort`/
//! `Distinct`/`Limit` nodes instead of the quirks of [`SelectStmt`]. Rewrites
//! such as [`pushdown`] then operate on this algebra, and the physical layer
//! lowers the result. Access-path choice (index vs scan, hash vs nested loop)
//! is deliberately *not* here: it stays in lowering.
//!
//! `Aggregate` is still one logical node for a grouped query; lowering splits
//! it into a physical `Aggregate` plus separate HAVING/ORDER BY/projection/
//! DISTINCT/LIMIT operators. Splitting the logical node itself is a later step.

use crate::catalog::Schema;
use crate::sql::ast::{Expr, JoinKind, Limit, SelectItem, SelectStmt, TableRef};
use crate::{Database, Result};

use super::eval::{expr_has_column, expr_has_subquery};
use super::operator::{build_from_source, combine_and, items_have_aggregate, join_clauses, split_conjuncts};

/// One node of the logical plan for a SELECT.
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
    /// Grouping/aggregation: one output row per group, with the aggregate
    /// results appended (see the physical `Aggregate`).
    Aggregate {
        input: Box<LogicalOperator>,
        group_by: Vec<Expr>,
        aggregates: Vec<Expr>,
    },
    /// A `HAVING` filter above an [`LogicalOperator::Aggregate`].
    Having { input: Box<LogicalOperator>, predicate: Expr },
    Project { input: Box<LogicalOperator>, items: Vec<SelectItem> },
    Sort { input: Box<LogicalOperator>, order_by: Vec<(Expr, bool)> },
    Distinct { input: Box<LogicalOperator> },
    Limit { input: Box<LogicalOperator>, limit: Limit },
}

/// Translates a SELECT into a logical plan. Returns `None` for shapes handled
/// directly by the planner (no FROM, or a UNION chain).
pub(crate) fn translate(select: &SelectStmt) -> Option<LogicalOperator> {
    if select.from.is_empty() || !select.set_ops.is_empty() {
        return None;
    }
    let mut node = logical_from(select)?;
    // Grouped/aggregate queries: Aggregate, then HAVING / ORDER BY / project /
    // DISTINCT / LIMIT as standard nodes.
    if !select.group_by.is_empty()
        || select.having.is_some()
        || items_have_aggregate(&select.items)
    {
        let mut tail = LogicalOperator::Aggregate {
            input: Box::new(node),
            group_by: select.group_by.clone(),
            aggregates: aggregate_list(select),
        };
        if let Some(having) = &select.having {
            tail = LogicalOperator::Having {
                input: Box::new(tail),
                predicate: having.clone(),
            };
        }
        if !select.order_by.is_empty() {
            tail = LogicalOperator::Sort {
                input: Box::new(tail),
                order_by: select.order_by.clone(),
            };
        }
        tail = LogicalOperator::Project {
            input: Box::new(tail),
            items: select.items.clone(),
        };
        if select.distinct {
            tail = LogicalOperator::Distinct { input: Box::new(tail) };
        }
        if let Some(limit) = &select.limit {
            tail = LogicalOperator::Limit { input: Box::new(tail), limit: limit.clone() };
        }
        return Some(tail);
    }
    if !select.order_by.is_empty() {
        node = LogicalOperator::Sort {
            input: Box::new(node),
            order_by: select.order_by.clone(),
        };
    }
    node = LogicalOperator::Project {
        input: Box::new(node),
        items: select.items.clone(),
    };
    if select.distinct {
        node = LogicalOperator::Distinct { input: Box::new(node) };
    }
    if let Some(limit) = &select.limit {
        node = LogicalOperator::Limit { input: Box::new(node), limit: limit.clone() };
    }
    Some(node)
}

/// Translates a SELECT's FROM / WHERE / JOIN region into a logical plan. The
/// WHERE clause becomes one `Filter` above the join tree; it is not yet pushed.
fn logical_from(select: &SelectStmt) -> Option<LogicalOperator> {
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

/// The distinct aggregate expressions in a SELECT's items, HAVING and ORDER BY,
/// in first-seen order. Matches what lowering extracts, so `#aggN` indices line
/// up.
fn aggregate_list(select: &SelectStmt) -> Vec<Expr> {
    let mut refs: Vec<&Expr> = Vec::new();
    for item in &select.items {
        match item {
            SelectItem::Expr(e) | SelectItem::Aliased(e, _) => refs.push(e),
            SelectItem::Star => {}
        }
    }
    if let Some(having) = &select.having {
        refs.push(having);
    }
    for (e, _) in &select.order_by {
        refs.push(e);
    }
    super::aggregate::extract_aggregates(&refs)
}

/// The output schema of a logical node, `None` when a source cannot be built
/// (the caller then falls back to the materialized executor).
fn schema(db: &Database, node: &LogicalOperator) -> Result<Option<Schema>> {
    match node {
        LogicalOperator::Scan(tref) => {
            Ok(build_from_source(db, tref)?.map(|op| op.schema().clone()))
        }
        LogicalOperator::Filter { input, .. }
        | LogicalOperator::Having { input, .. }
        | LogicalOperator::Aggregate { input, .. }
        | LogicalOperator::Project { input, .. }
        | LogicalOperator::Sort { input, .. }
        | LogicalOperator::Distinct { input }
        | LogicalOperator::Limit { input, .. } => schema(db, input),
        LogicalOperator::Join { left, right, .. } => {
            let (Some(mut left), Some(right)) = (schema(db, left)?, schema(db, right)?) else {
                return Ok(None);
            };
            left.columns.extend(right.columns);
            Ok(Some(left))
        }
    }
}

/// Applies the logical rewrites to the plan (currently predicate pushdown).
pub(crate) fn optimize(
    db: &Database,
    node: LogicalOperator,
) -> Result<Option<LogicalOperator>> {
    Ok(match node {
        node @ (LogicalOperator::Scan(_)
        | LogicalOperator::Filter { .. }
        | LogicalOperator::Join { .. }) => pushdown_region(db, node)?,
        LogicalOperator::Aggregate { input, group_by, aggregates } => {
            optimize(db, *input)?.map(|n| LogicalOperator::Aggregate {
                input: Box::new(n),
                group_by,
                aggregates,
            })
        }
        LogicalOperator::Having { input, predicate } => {
            optimize(db, *input)?.map(|n| LogicalOperator::Having {
                input: Box::new(n),
                predicate,
            })
        }
        LogicalOperator::Project { input, items } => {
            optimize(db, *input)?.map(|n| LogicalOperator::Project {
                input: Box::new(n),
                items,
            })
        }
        LogicalOperator::Sort { input, order_by } => {
            optimize(db, *input)?.map(|n| LogicalOperator::Sort {
                input: Box::new(n),
                order_by,
            })
        }
        LogicalOperator::Distinct { input } => {
            optimize(db, *input)?.map(|n| LogicalOperator::Distinct { input: Box::new(n) })
        }
        LogicalOperator::Limit { input, limit } => optimize(db, *input)?.map(|n| {
            LogicalOperator::Limit { input: Box::new(n), limit }
        }),
    })
}

/// Pushes WHERE conjuncts that reference a single source onto that source.
/// Only inner/comma join chains are eligible: below an outer join the
/// null-extension would change which rows are produced.
fn pushdown_region(
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
        _ => false,
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
        _ => {}
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
        other => other,
    }
}

/// Renders an indented tree of `node`, two spaces per depth, newline-terminated.
pub(crate) fn logical_tree(node: &LogicalOperator) -> String {
    fn walk(node: &LogicalOperator, depth: usize, out: &mut String) {
        for _ in 0..depth {
            out.push_str("  ");
        }
        out.push_str(&label(node));
        out.push('\n');
        children(node, &mut |child| walk(child, depth + 1, out));
    }
    fn label(node: &LogicalOperator) -> String {
        match node {
            LogicalOperator::Scan(tref) => match &tref.alias {
                Some(alias) => format!("Scan {} as {alias}", tref.name),
                None => format!("Scan {}", tref.name),
            },
            LogicalOperator::Filter { predicate, .. } => format!("Filter {predicate}"),
            LogicalOperator::Having { predicate, .. } => format!("Having {predicate}"),
            LogicalOperator::Join { kind, .. } => format!("Join {kind:?}"),
            LogicalOperator::Aggregate { .. } => "Aggregate".to_string(),
            LogicalOperator::Project { items, .. } => format!("Project cols={}", items.len()),
            LogicalOperator::Sort { order_by, .. } => format!("Sort keys={}", order_by.len()),
            LogicalOperator::Distinct { .. } => "Distinct".to_string(),
            LogicalOperator::Limit { limit, .. } => {
                format!("Limit count={}", limit.count)
            }
        }
    }
    fn children(node: &LogicalOperator, f: &mut impl FnMut(&LogicalOperator)) {
        match node {
            LogicalOperator::Scan(_) => {}
            LogicalOperator::Filter { input, .. }
            | LogicalOperator::Having { input, .. }
            | LogicalOperator::Aggregate { input, .. }
            | LogicalOperator::Project { input, .. }
            | LogicalOperator::Sort { input, .. }
            | LogicalOperator::Distinct { input }
            | LogicalOperator::Limit { input, .. } => f(input),
            LogicalOperator::Join { left, right, .. } => {
                f(left);
                f(right);
            }
        }
    }
    let mut out = String::new();
    walk(node, 0, &mut out);
    out
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
