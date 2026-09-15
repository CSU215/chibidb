//! Lowering: turn an optimized `LogicalOperator` tree into physical operators.
//! This is where access-path choice (index vs full scan, ordered scan, hash vs
//! nested-loop join) happens; the logical layer stays physical-agnostic.

use std::collections::HashSet;

use crate::catalog::{ColumnDesc, Schema};
use crate::sql::ast::{BinOp, Expr, JoinKind, SelectItem, TableRef};
use crate::value::{DataType, Value};
use crate::{Database, Error, Result};

use crate::exec::aggregate::{self, expr_has_aggregate};
use crate::exec::eval::{eval_const, expr_has_column};
use crate::exec::operator::{
    ConstantScan, Distinct, Filter, HashJoin, HashKeys, IndexScan, Limit, NestedLoopJoin,
    PhysicalOperator, Project, Sort, TableScan, Union, ViewScan,
};

use super::access::resolved_order_column;
use super::logical::LogicalOperator;
use super::util::{combine_and, items_have_aggregate, split_conjuncts};

/// What lowering needs from the enclosing query, accumulated as it descends the
/// logical plan: `Project` supplies the items, `Sort` the order keys, `Having`
/// the predicate and `Aggregate` the grouping. Keeping this context explicit
/// lets lowering consume a bare `LogicalOperator` instead of the `SelectStmt`.
#[derive(Clone, Copy, Default)]
struct LowerCtx<'a> {
    items: &'a [SelectItem],
    order_by: &'a [(Expr, bool)],
    having: Option<&'a Expr>,
    group_by: &'a [Expr],
}

/// Dtype of a simple column reference, used to reject hash keys whose numerics
/// would need coercion (the index key encoding is type-sensitive).
fn column_dtype(schema: &Schema, expr: &Expr) -> Option<DataType> {
    match expr {
        Expr::Column(name) => {
            schema.columns.iter().find(|c| &c.name == name).map(|c| c.dtype)
        }
        Expr::QualifiedColumn(owner, name) => schema
            .columns
            .iter()
            .find(|c| c.owner.as_deref() == Some(owner) && &c.name == name)
            .map(|c| c.dtype),
        _ => None,
    }
}

fn compatible(a: DataType, b: DataType) -> bool {
    matches!(
        (a, b),
        (DataType::Int, DataType::Int)
            | (DataType::Float, DataType::Float)
            | (DataType::Date, DataType::Date)
            | (DataType::Text, DataType::Text)
            | (DataType::Char(_), DataType::Char(_))
    )
}

/// Extracts equi-join key pairs from `ON a.k = b.k [and ...]`. Returns `None`
/// unless at least one pair of plain, same-typed columns is found.
fn analyze_hash_join(
    kind: JoinKind,
    condition: Option<&Expr>,
    left: &Schema,
    right: &Schema,
) -> Option<HashKeys> {
    if kind == JoinKind::Cross {
        return None;
    }
    let condition = condition?;
    let conjuncts: Vec<Expr> = split_conjuncts(condition).into_iter().cloned().collect();
    let (left_keys, right_keys, residual) = extract_hash_keys(&conjuncts, left, right);
    if left_keys.is_empty() {
        return None;
    }
    Some(HashKeys { left_keys, right_keys, residual: combine_and(residual) })
}

/// Splits `conjuncts` into equi-join key pairs between `left` and `right`, plus
/// the predicates that must still be evaluated. A conjunct becomes a key only
/// when its two sides resolve to compatible columns of the respective schemas,
/// so callers can safely drop the returned keys from a filter.
fn extract_hash_keys(
    conjuncts: &[Expr],
    left: &Schema,
    right: &Schema,
) -> (Vec<Expr>, Vec<Expr>, Vec<Expr>) {
    let mut left_keys = Vec::new();
    let mut right_keys = Vec::new();
    let mut kept = Vec::new();
    for conjunct in conjuncts {
        if let Expr::Binary(BinOp::Eq, a, b) = conjunct {
            if let (Some(ld), Some(rd)) = (column_dtype(left, a), column_dtype(right, b))
                && compatible(ld, rd)
            {
                left_keys.push((**a).clone());
                right_keys.push((**b).clone());
                continue;
            }
            if let (Some(ld), Some(rd)) = (column_dtype(left, b), column_dtype(right, a))
                && compatible(ld, rd)
            {
                left_keys.push((**b).clone());
                right_keys.push((**a).clone());
                continue;
            }
        }
        kept.push(conjunct.clone());
    }
    (left_keys, right_keys, kept)
}

/// Builds a scan for one FROM entry: a table scan or a view sub-plan. The view
/// definition is parsed and planned here, so the `ViewScan` operator only ever
/// receives a ready child plan.
pub(crate) fn build_from_source(
    db: &Database,
    tref: &TableRef,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    let owner = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
    if let Some(sql) = db.catalog().view(&tref.name).cloned() {
        let stmts = crate::sql::parser::parse(&sql)?;
        let Some(crate::sql::ast::Stmt::Select(select)) = stmts.into_iter().next() else {
            return Ok(None);
        };
        let Some(child) = super::plan_select(db, &select)? else {
            return Ok(None);
        };
        return Ok(Some(Box::new(ViewScan::new(child, &owner))));
    }
    Ok(Some(Box::new(TableScan::with_owner(db, &tref.name, &owner)?)))
}

/// A lowered FROM region plus the ORDER BY column an ordered index scan already
/// provides, so the caller can skip the sort.
type LoweredFrom = (Box<dyn PhysicalOperator>, Option<String>);

/// Lowers a logical Scan/Filter/Join region into a physical tree, reproducing
/// the access-path choices the builder made directly before. Returns `None`
/// when a source cannot be built, so the caller falls back to the materialized
/// executor.
fn lower_region(
    db: &Database,
    node: &LogicalOperator,
    ctx: &LowerCtx<'_>,
) -> Result<Option<LoweredFrom>> {
    if let Some((tref, predicate)) = single_scan(node) {
        let Some((mut op, ordered)) = lower_single_table(db, tref, predicate, ctx)? else {
            return Ok(None);
        };
        if let Some(predicate) = predicate {
            op = Box::new(Filter::new(op, predicate.clone()));
        }
        return Ok(Some((op, ordered)));
    }

    // Multi-table: WHERE conjuncts left after pushdown feed comma-join hash
    // keys; whatever no join consumes becomes the residual filter above the tree.
    let (root, mut residual) = match node {
        LogicalOperator::Filter { input, predicate } => (
            input.as_ref(),
            split_conjuncts(predicate).into_iter().cloned().collect::<Vec<Expr>>(),
        ),
        other => (other, Vec::new()),
    };
    let Some(mut op) = lower_join_tree(db, root, &mut residual)? else {
        return Ok(None);
    };
    if let Some(predicate) = combine_and(residual) {
        op = Box::new(Filter::new(op, predicate));
    }
    Ok(Some((op, None)))
}

/// The single-table part of a region: a `Scan`, or a `Filter` directly over it.
/// Anything else is a multi-source region.
fn single_scan(node: &LogicalOperator) -> Option<(&TableRef, Option<&Expr>)> {
    match node {
        LogicalOperator::Scan(tref) => Some((tref, None)),
        LogicalOperator::Filter { input, predicate } => match input.as_ref() {
            LogicalOperator::Scan(tref) => Some((tref, Some(predicate))),
            _ => None,
        },
        _ => None,
    }
}

/// The single-table access path: view, best index, ordered index, full scan.
fn lower_single_table(
    db: &Database,
    tref: &TableRef,
    predicate: Option<&Expr>,
    ctx: &LowerCtx<'_>,
) -> Result<Option<LoweredFrom>> {
    if db.catalog().view(&tref.name).is_some() {
        return Ok(build_from_source(db, tref)?.map(|source| (source, None)));
    }
    let owner = tref.alias.as_deref().unwrap_or(&tref.name);
    // Access-path selection may only reuse an index's order when the query is
    // not grouped/aggregated and the ORDER BY resolves to that column.
    let orderable = ctx.group_by.is_empty() && !items_have_aggregate(ctx.items);
    let ordered_column = resolved_order_column(ctx.items, ctx.order_by);
    if let Some(plan) = super::plan_index_scan(db, &tref.name, predicate)? {
        let column = plan.column.clone();
        let mut scan = IndexScan::from_plan(db, &tref.name, owner, plan)?;
        if orderable && ordered_column.as_deref() == Some(column.as_str()) {
            scan.mark_ordered();
        }
        return Ok(Some((Box::new(scan), Some(column))));
    }
    // An index on the ORDER BY column can supply the order even without a WHERE
    // clause, skipping the sort.
    if orderable
        && let Some(column) = &ordered_column
        && let Some(file) = super::ordered_index_file(db, &tref.name, column)?
    {
        let scan = IndexScan::from_cursor(db, &tref.name, owner, column, file)?;
        let column = scan.ordered_column().to_string();
        return Ok(Some((Box::new(scan), Some(column))));
    }
    // sequential scans may skip large objects the query never reads
    let keep = lob_keep(
        ctx.items,
        predicate,
        ctx.group_by,
        ctx.having,
        ctx.order_by,
        &db.catalog().table(&tref.name)?.schema.columns,
        owner,
        &tref.name,
    );
    Ok(Some((Box::new(TableScan::with_owner_keep(db, &tref.name, owner, keep)?), None)))
}

/// Lowers a join/scan tree. `residual` holds the WHERE conjuncts still
/// available to form comma-join hash keys; consumed ones are removed.
fn lower_join_tree(
    db: &Database,
    node: &LogicalOperator,
    residual: &mut Vec<Expr>,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    match node {
        LogicalOperator::Scan(tref) => build_from_source(db, tref),
        LogicalOperator::Filter { input, predicate } => {
            let Some(inner) = lower_join_tree(db, input, residual)? else {
                return Ok(None);
            };
            Ok(Some(Box::new(Filter::new(inner, predicate.clone()))))
        }
        LogicalOperator::Join { left, right, kind, on } => {
            let lop = lower_join_tree(db, left, residual)?;
            let rop = lower_join_tree(db, right, residual)?;
            let (Some(lop), Some(rop)) = (lop, rop) else {
                return Ok(None);
            };
            let keys = match on {
                Some(on) => analyze_hash_join(*kind, Some(on), lop.schema(), rop.schema()),
                None if *kind == JoinKind::Cross => {
                    let (left_keys, right_keys, kept) =
                        extract_hash_keys(residual, lop.schema(), rop.schema());
                    if left_keys.is_empty() {
                        None
                    } else {
                        *residual = kept;
                        Some(HashKeys { left_keys, right_keys, residual: None })
                    }
                }
                None => None,
            };
            Ok(Some(match keys {
                Some(keys) => Box::new(HashJoin::new(lop, rop, *kind, keys)),
                None => Box::new(NestedLoopJoin::new(lop, rop, *kind, on.clone())?),
            }))
        }
        // Only Scan/Filter/Join appear inside a region.
        _ => Ok(None),
    }
}

/// Lowers a logical plan to physical operators. This is the only place that
/// picks access paths and expands widening (`*`, ORDER BY aliases), all from the
/// logical plan plus the query context it carries.
pub(crate) fn lower(
    db: &Database,
    node: &LogicalOperator,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    lower_node(db, node, &LowerCtx::default())
}

fn lower_node(
    db: &Database,
    node: &LogicalOperator,
    ctx: &LowerCtx<'_>,
) -> Result<Option<Box<dyn PhysicalOperator>>> {
    Ok(match node {
        LogicalOperator::Scan(_)
        | LogicalOperator::Filter { .. }
        | LogicalOperator::Join { .. } => lower_region(db, node, ctx)?.map(|(op, _)| op),
        LogicalOperator::Constant => Some(Box::new(ConstantScan::new())),
        LogicalOperator::Aggregate { input, group_by, aggregates } => {
            for g in group_by {
                if expr_has_aggregate(g) {
                    return Err(Error::Runtime(
                        "aggregate functions are not allowed in group by".into(),
                    ));
                }
            }
            if group_by.is_empty() {
                for item in ctx.items {
                    if let SelectItem::Expr(e) | SelectItem::Aliased(e, _) = item
                        && expr_has_column(e)
                    {
                        return Err(Error::Runtime(
                            "column must appear in group by or aggregate".into(),
                        ));
                    }
                }
            }
            let child_ctx = LowerCtx { group_by, ..*ctx };
            let Some(op) = lower_node(db, input, &child_ctx)? else {
                return Ok(None);
            };
            let input_schema = op.schema().clone();
            // Columns the representative row must carry: group keys, the
            // projection's base columns, HAVING/ORDER BY, and aggregate args.
            let (exprs, _) = build_projection(&input_schema, ctx.items);
            let mut needed_refs: Vec<&Expr> = group_by.iter().collect();
            needed_refs.extend(exprs.iter());
            if let Some(having) = ctx.having {
                needed_refs.push(having);
            }
            for (e, _) in ctx.order_by {
                needed_refs.push(e);
            }
            for agg in aggregates {
                if let Expr::Aggregate(_, Some(arg), _) = agg {
                    needed_refs.push(arg);
                }
            }
            let needed = aggregate::referenced_columns(&input_schema, &needed_refs);
            Some(Box::new(aggregate::Aggregate::new(
                op,
                input_schema,
                group_by.clone(),
                aggregates.clone(),
                needed,
            )))
        }
        LogicalOperator::Having { input, predicate } => {
            let child_ctx = LowerCtx { having: Some(predicate), ..*ctx };
            let Some(op) = lower_node(db, input, &child_ctx)? else {
                return Ok(None);
            };
            let predicate = match find_aggregate_parts(op.as_ref()) {
                Some((_, aggregates)) => aggregate::rewrite_aggregates(predicate, aggregates),
                None => predicate.clone(),
            };
            Some(Box::new(Filter::new(op, predicate)))
        }
        LogicalOperator::Project { input, items } => {
            let child_ctx = LowerCtx { items, ..*ctx };
            let Some(op) = lower_node(db, input, &child_ctx)? else {
                return Ok(None);
            };
            let (exprs, headers) = match find_aggregate_parts(op.as_ref()) {
                Some((input_schema, aggregates)) => {
                    let (exprs, headers) = build_projection(input_schema, items);
                    let exprs = exprs
                        .iter()
                        .map(|e| aggregate::rewrite_aggregates(e, aggregates))
                        .collect();
                    (exprs, headers)
                }
                None => build_projection(op.schema(), items),
            };
            Some(Box::new(Project::new(op, exprs, headers)))
        }
        LogicalOperator::Sort { input, order_by } => {
            let child_ctx = LowerCtx { order_by, ..*ctx };
            // A bare region below may already provide the order via an index.
            if matches!(
                input.as_ref(),
                LogicalOperator::Scan(_)
                    | LogicalOperator::Filter { .. }
                    | LogicalOperator::Join { .. }
            ) {
                let Some((op, ordered_by)) = lower_region(db, input, &child_ctx)? else {
                    return Ok(None);
                };
                let skip = ordered_by.as_deref().is_some_and(|column| {
                    resolved_order_column(ctx.items, order_by).as_deref() == Some(column)
                });
                if skip {
                    return Ok(Some(op));
                }
                let order = aggregate::resolve_order_aliases(order_by, ctx.items);
                return Ok(Some(Box::new(Sort::new(op, order))));
            }
            let Some(op) = lower_node(db, input, &child_ctx)? else {
                return Ok(None);
            };
            let (order, items) = match find_aggregate_parts(op.as_ref()) {
                Some((_, aggregates)) => (
                    order_by
                        .iter()
                        .map(|(e, desc)| (aggregate::rewrite_aggregates(e, aggregates), *desc))
                        .collect(),
                    ctx.items
                        .iter()
                        .map(|item| rewrite_item(item, aggregates))
                        .collect(),
                ),
                None => (order_by.clone(), ctx.items.to_vec()),
            };
            let order = aggregate::resolve_order_aliases(&order, &items);
            Some(Box::new(Sort::new(op, order)))
        }
        LogicalOperator::Distinct { input } => {
            let Some(op) = lower_node(db, input, ctx)? else {
                return Ok(None);
            };
            Some(Box::new(Distinct::new(op)))
        }
        LogicalOperator::Limit { input, limit } => {
            let Some(op) = lower_node(db, input, ctx)? else {
                return Ok(None);
            };
            let offset = limit_bound(limit.offset.as_ref())?;
            let count = limit_bound(Some(&limit.count))?;
            Some(Box::new(Limit::new(op, offset, Some(count))))
        }
        LogicalOperator::Union { inputs, order_by, limit } => {
            let mut physical = Vec::with_capacity(inputs.len());
            for (all, input) in inputs {
                let Some(op) = lower_node(db, input, &LowerCtx::default())? else {
                    return Ok(None);
                };
                physical.push((*all, op));
            }
            Some(Box::new(Union::new(physical, order_by.clone(), limit.clone())))
        }
        LogicalOperator::Insert(i) => {
            Some(Box::new(crate::exec::dml::InsertOp::new(i.clone())))
        }
        LogicalOperator::Update(u) => {
            Some(Box::new(crate::exec::dml::UpdateOp::new(u.clone())))
        }
        LogicalOperator::Delete(d) => {
            Some(Box::new(crate::exec::dml::DeleteOp::new(d.clone())))
        }
    })
}

/// Finds the aggregate parts of `op`, walking single-child wrappers (Sort,
/// Filter, ...) down to the `Aggregate` beneath.
fn find_aggregate_parts(op: &dyn PhysicalOperator) -> Option<(&Schema, &[Expr])> {
    if let Some(parts) = op.aggregate_parts() {
        return Some(parts);
    }
    op.children().into_iter().find_map(find_aggregate_parts)
}

fn rewrite_item(item: &SelectItem, aggregates: &[Expr]) -> SelectItem {
    match item {
        SelectItem::Expr(e) => SelectItem::Expr(aggregate::rewrite_aggregates(e, aggregates)),
        SelectItem::Aliased(e, alias) => {
            SelectItem::Aliased(aggregate::rewrite_aggregates(e, aggregates), alias.clone())
        }
        SelectItem::Star => SelectItem::Star,
    }
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

/// Per-column flags marking which base columns a single-table SELECT reads.
/// Returns `None` (do not prune) whenever the analysis cannot be certain:
/// a `*` projection, a subquery, or a foreign qualifier.
#[allow(clippy::too_many_arguments)]
fn lob_keep(
    items: &[SelectItem],
    selection: Option<&Expr>,
    group_by: &[Expr],
    having: Option<&Expr>,
    order_by: &[(Expr, bool)],
    columns: &[ColumnDesc],
    owner: &str,
    table: &str,
) -> Option<Vec<bool>> {
    let mut needed: HashSet<String> = HashSet::new();
    let mut safe = true;
    let visit = |expr: &Expr, needed: &mut HashSet<String>, safe: &mut bool| {
        if !collect_column_refs(expr, owner, table, needed) {
            *safe = false;
        }
    };
    for item in items {
        match item {
            SelectItem::Star => return None,
            SelectItem::Expr(e) | SelectItem::Aliased(e, _) => {
                visit(e, &mut needed, &mut safe);
            }
        }
    }
    if let Some(selection) = selection {
        visit(selection, &mut needed, &mut safe);
    }
    for expr in group_by {
        visit(expr, &mut needed, &mut safe);
    }
    if let Some(having) = having {
        visit(having, &mut needed, &mut safe);
    }
    for (expr, _) in order_by {
        visit(expr, &mut needed, &mut safe);
    }
    if !safe {
        return None;
    }
    Some(columns.iter().map(|c| needed.contains(&c.name)).collect())
}

/// Records base-column references; returns `false` when it sees something it
/// cannot classify (a subquery or a foreign qualifier), forcing full decoding.
fn collect_column_refs(expr: &Expr, owner: &str, table: &str, needed: &mut HashSet<String>) -> bool {
    match expr {
        Expr::Column(name) => {
            needed.insert(name.clone());
            true
        }
        Expr::QualifiedColumn(qual, name) => {
            if qual == owner || qual == table {
                needed.insert(name.clone());
                true
            } else {
                false
            }
        }
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Null | Expr::Value(_) => true,
        Expr::Unary(_, a) => collect_column_refs(a, owner, table, needed),
        Expr::Binary(_, a, b) => {
            collect_column_refs(a, owner, table, needed)
                & collect_column_refs(b, owner, table, needed)
        }
        Expr::IsNull(a, _) => collect_column_refs(a, owner, table, needed),
        Expr::Like { expr, pattern, .. } => {
            collect_column_refs(expr, owner, table, needed)
                & collect_column_refs(pattern, owner, table, needed)
        }
        Expr::Function(_, args) => args
            .iter()
            .all(|a| collect_column_refs(a, owner, table, needed)),
        Expr::Aggregate(_, Some(a), _) => collect_column_refs(a, owner, table, needed),
        Expr::Aggregate(_, None, _) => true,
        Expr::InSubquery { .. } | Expr::Exists { .. } | Expr::ScalarSubquery(_) => false,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDesc, Schema};

    fn schema(owner: &str, name: &str, dtype: DataType) -> Schema {
        Schema {
            columns: vec![ColumnDesc::plain(Some(owner.to_string()), name.to_string(), dtype)],
        }
    }

    fn eq(left_owner: &str, right_owner: &str) -> Expr {
        Expr::Binary(
            BinOp::Eq,
            Box::new(Expr::QualifiedColumn(left_owner.into(), "id".into())),
            Box::new(Expr::QualifiedColumn(right_owner.into(), "id".into())),
        )
    }

    #[test]
    fn extracts_equi_join_keys() {
        let left = schema("a", "id", DataType::Int);
        let right = schema("b", "id", DataType::Int);
        let keys = analyze_hash_join(JoinKind::Inner, Some(&eq("a", "b")), &left, &right).unwrap();
        assert_eq!(keys.left_keys.len(), 1);
        assert_eq!(keys.right_keys.len(), 1);
        assert!(keys.residual.is_none());
    }

    #[test]
    fn rejects_mismatched_numeric_key_types() {
        let left = schema("a", "id", DataType::Int);
        let right = schema("b", "id", DataType::Float);
        assert!(analyze_hash_join(JoinKind::Inner, Some(&eq("a", "b")), &left, &right).is_none());
    }

    #[test]
    fn cross_join_is_not_hashable() {
        let left = schema("a", "id", DataType::Int);
        let right = schema("b", "id", DataType::Int);
        assert!(analyze_hash_join(JoinKind::Cross, Some(&eq("a", "b")), &left, &right).is_none());
    }

    #[test]
    fn non_equi_predicate_is_not_hashable() {
        let left = schema("a", "id", DataType::Int);
        let right = schema("b", "id", DataType::Int);
        let condition = Expr::Binary(
            BinOp::Gt,
            Box::new(Expr::QualifiedColumn("a".into(), "id".into())),
            Box::new(Expr::QualifiedColumn("b".into(), "id".into())),
        );
        assert!(
            analyze_hash_join(JoinKind::Inner, Some(&condition), &left, &right).is_none()
        );
    }
}