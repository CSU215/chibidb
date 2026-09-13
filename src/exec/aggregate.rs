use std::collections::HashMap;

use crate::ast::{AggFunc, BinOp, Expr, Limit, SelectItem, SelectStmt};
use crate::catalog::Schema;
use crate::trx::TrxState;
use crate::value::Value;
use crate::{Database, Error, Result};

use super::chunk::{Chunk, Column};
use super::eval::{
    cmp_values, eval, eval_binary, eval_const, expr_has_column, type_mismatch, EvalCtx,
};
use super::operator::{ExecContext, PhysicalOperator};
use super::subquery::eval_bound;

pub(crate) fn expr_has_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Aggregate(..) => true,
        Expr::Unary(_, e) => expr_has_aggregate(e),
        Expr::Binary(_, l, r) => expr_has_aggregate(l) || expr_has_aggregate(r),
        Expr::IsNull(e, _) => expr_has_aggregate(e),
        Expr::Like { expr, pattern, .. } => {
            expr_has_aggregate(expr) || expr_has_aggregate(pattern)
        }
        Expr::Function(_, args) => args.iter().any(expr_has_aggregate),
        _ => false,
    }
}

pub(crate) fn eval_aggregate(
    func: AggFunc,
    arg: Option<&Expr>,
    distinct: bool,
    schema: &Schema,
    rows: &[Vec<Value>],
) -> Result<Value> {
    let mut vals: Vec<Value> = match arg {
        None => vec![],
        Some(e) => {
            let mut vals = Vec::with_capacity(rows.len());
            for row in rows {
                match eval(e, Some(&EvalCtx::row(schema, row)))? {
                    Value::Null => {}
                    v => vals.push(v),
                }
            }
            vals
        }
    };
    if distinct {
        let mut seen: Vec<Value> = Vec::new();
        vals.retain(|v| {
            if seen.contains(v) {
                false
            } else {
                seen.push(v.clone());
                true
            }
        });
    }
    match func {
        AggFunc::Count => Ok(Value::Int(match arg {
            None => rows.len() as i64,
            Some(_) => vals.len() as i64,
        })),
        AggFunc::Sum => {
            let mut acc: Option<Value> = None;
            for v in vals {
                acc = Some(match acc {
                    None => v,
                    Some(a) => eval_binary(BinOp::Add, a, v)?,
                });
            }
            Ok(acc.unwrap_or(Value::Null))
        }
        AggFunc::Avg => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            let mut total = 0.0f64;
            for v in &vals {
                match v {
                    Value::Int(n) => total += *n as f64,
                    Value::Float(x) => total += *x,
                    _ => return Err(type_mismatch()),
                }
            }
            Ok(Value::Float(total / vals.len() as f64))
        }
        AggFunc::Min | AggFunc::Max => {
            let mut best: Option<&Value> = None;
            for v in &vals {
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        let ord = cmp_values(b, v)?.ok_or_else(type_mismatch)?;
                        let take = match func {
                            AggFunc::Min => ord == std::cmp::Ordering::Greater,
                            _ => ord == std::cmp::Ordering::Less,
                        };
                        if take {
                            v
                        } else {
                            b
                        }
                    }
                });
            }
            Ok(best.cloned().unwrap_or(Value::Null))
        }
    }
}

/// Core of grouped/aggregate execution: group, having, order groups, project
/// (with group context), distinct and limit. Used by the `GroupBy` operator.
#[allow(clippy::too_many_arguments)]
pub(crate) fn grouped_select_rows(
    db: &Database,
    trx: &mut TrxState,
    outer: Option<&EvalCtx>,
    schema: &Schema,
    s: &SelectStmt,
    filtered: Vec<Vec<Value>>,
    exprs: Vec<Expr>,
) -> Result<Vec<Vec<Value>>> {
    for g in &s.group_by {
        if expr_has_aggregate(g) {
            return Err(Error::Runtime("aggregate functions are not allowed in group by".into()));
        }
    }
    if s.group_by.is_empty() {
        for it in &s.items {
            if let SelectItem::Expr(e) | SelectItem::Aliased(e, _) = it
                && expr_has_column(e) {
                    return Err(Error::Runtime(
                        "column must appear in group by or aggregate".into(),
                    ));
                }
        }
    }
    let mut groups: Vec<(Vec<Value>, Vec<Vec<Value>>)> = Vec::new();
    if s.group_by.is_empty() {
        groups.push((vec![], filtered));
    } else {
        for row in filtered {
            let mut ctx = EvalCtx::row(schema, &row);
            ctx.parent = outer;
            let mut key = Vec::with_capacity(s.group_by.len());
            for g in &s.group_by {
                key.push(eval_bound(db, trx, g, Some(&ctx))?);
            }
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, rows)) => rows.push(row),
                None => groups.push((key, vec![row])),
            }
        }
    }
    let mut surviving: Vec<Vec<Vec<Value>>> = Vec::new();
    for (_, group_rows) in groups {
        if let Some(having) = &s.having {
            let mut ctx = EvalCtx::group(schema, &group_rows);
            ctx.parent = outer;
            match eval_bound(db, trx, having, Some(&ctx))? {
                Value::Bool(true) => {}
                Value::Bool(false) | Value::Null => continue,
                _ => {
                    return Err(Error::Runtime(
                        "having clause must evaluate to boolean".into(),
                    ))
                }
            }
        }
        surviving.push(group_rows);
    }
    if !s.order_by.is_empty() {
        sort_groups(db, trx, outer, schema, &mut surviving, &s.order_by, &s.items)?;
    }
    let mut out_rows = Vec::new();
    for group_rows in surviving {
        let mut ctx = EvalCtx::group(schema, &group_rows);
        ctx.parent = outer;
        let mut out_row = Vec::with_capacity(exprs.len());
        for e in &exprs {
            out_row.push(eval_bound(db, trx, e, Some(&ctx))?);
        }
        out_rows.push(out_row);
    }
    if s.distinct {
        dedup_rows(&mut out_rows);
    }
    apply_limit(&mut out_rows, &s.limit)?;
    Ok(out_rows)
}

/// DISTINCT: keep the first occurrence of every projected row; NULLs are
/// equal for dedup purposes (SQL semantics).
pub(crate) fn dedup_rows(out_rows: &mut Vec<Vec<Value>>) {
    let mut seen: Vec<Vec<Value>> = Vec::new();
    out_rows.retain(|row| {
        if seen.iter().any(|s| s == row) {
            false
        } else {
            seen.push(row.clone());
            true
        }
    });
}

pub(crate) fn apply_limit(out_rows: &mut Vec<Vec<Value>>, limit: &Option<Limit>) -> Result<()> {
    let Some(limit) = limit else {
        return Ok(());
    };
    let count = match eval_const(&limit.count)? {
        Value::Int(n) if n >= 0 => n as usize,
        _ => return Err(Error::Runtime("limit count must be a non-negative integer".into())),
    };
    let offset = match &limit.offset {
        Some(e) => match eval_const(e)? {
            Value::Int(n) if n >= 0 => n as usize,
            _ => {
                return Err(Error::Runtime(
                    "limit offset must be a non-negative integer".into(),
                ))
            }
        },
        None => 0,
    };
    *out_rows = out_rows.iter().skip(offset).take(count).cloned().collect();
    Ok(())
}

fn select_aliases(items: &[SelectItem]) -> Vec<(String, Expr)> {
    items
        .iter()
        .filter_map(|it| match it {
            SelectItem::Aliased(e, alias) => Some((alias.clone(), e.clone())),
            _ => None,
        })
        .collect()
}

/// ORDER BY may reference output aliases (e.g. `count(*) as total`).
fn resolve_order_expr<'a>(expr: &'a Expr, aliases: &'a [(String, Expr)]) -> &'a Expr {
    match expr {
        Expr::Column(c) => aliases
            .iter()
            .find(|(a, _)| a == c)
            .map(|(_, e)| e)
            .unwrap_or(expr),
        _ => expr,
    }
}

fn eval_sort_keys(
    db: &Database,
    trx: &mut TrxState,
    ctx: EvalCtx,
    order_by: &[(Expr, bool)],
    aliases: &[(String, Expr)],
) -> Result<Vec<Value>> {
    let mut keys = Vec::with_capacity(order_by.len());
    for (e, _) in order_by {
        keys.push(eval_bound(db, trx, resolve_order_expr(e, aliases), Some(&ctx))?);
    }
    Ok(keys)
}

pub(crate) fn cmp_sort_keys(a: &[Value], b: &[Value], order_by: &[(Expr, bool)]) -> std::cmp::Ordering {
    for ((_, desc), (va, vb)) in order_by.iter().zip(a.iter().zip(b.iter())) {
        // nulls sort as smallest: first on asc, last on desc
        let ord = match (va, vb) {
            (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
            (Value::Null, _) => std::cmp::Ordering::Less,
            (_, Value::Null) => std::cmp::Ordering::Greater,
            _ => cmp_values(va, vb).ok().flatten().unwrap_or(std::cmp::Ordering::Equal),
        };
        let ord = if *desc { ord.reverse() } else { ord };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

pub(crate) fn sort_rows(
    db: &Database,
    trx: &mut TrxState,
    outer: Option<&EvalCtx>,
    schema: &Schema,
    rows: &mut Vec<Vec<Value>>,
    order_by: &[(Expr, bool)],
    items: &[SelectItem],
) -> Result<()> {
    let aliases = select_aliases(items);
    let mut pairs: Vec<(Vec<Value>, Vec<Value>)> = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        let mut ctx = EvalCtx::row(schema, &row);
        ctx.parent = outer;
        let keys = eval_sort_keys(db, trx, ctx, order_by, &aliases)?;
        pairs.push((row, keys));
    }
    pairs.sort_by(|(_, ka), (_, kb)| cmp_sort_keys(ka, kb, order_by));
    rows.extend(pairs.into_iter().map(|(r, _)| r));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn sort_groups(
    db: &Database,
    trx: &mut TrxState,
    outer: Option<&EvalCtx>,
    schema: &Schema,
    groups: &mut Vec<Vec<Vec<Value>>>,
    order_by: &[(Expr, bool)],
    items: &[SelectItem],
) -> Result<()> {
    let aliases = select_aliases(items);
    let mut pairs: Vec<(Vec<Vec<Value>>, Vec<Value>)> = Vec::with_capacity(groups.len());
    for group in groups.drain(..) {
        let mut ctx = EvalCtx::group(schema, &group);
        ctx.parent = outer;
        let keys = eval_sort_keys(db, trx, ctx, order_by, &aliases)?;
        pairs.push((group, keys));
    }
    pairs.sort_by(|(_, ka), (_, kb)| cmp_sort_keys(ka, kb, order_by));
    groups.extend(pairs.into_iter().map(|(g, _)| g));
    Ok(())
}

/// A columnar aggregate for the common global shape: `SELECT agg(col)...
/// FROM t` with no GROUP BY, HAVING, DISTINCT, ORDER BY or LIMIT and every
/// projection a direct `count/sum/avg/min/max`.
///
/// Returns `Ok(None)` without consuming the child when the shape does not
/// qualify, so the caller can fall back to [`grouped_select_rows`]. The
/// accumulators mirror [`eval_aggregate`] (same comparators, same arithmetic,
/// same NULL rules) so results are identical to the row path.
pub(crate) fn chunk_global_aggregate(
    ctx: &mut ExecContext<'_>,
    schema: &Schema,
    child: &mut dyn PhysicalOperator,
    select: &SelectStmt,
    exprs: &[Expr],
) -> Result<Option<Vec<Vec<Value>>>> {
    if !select.group_by.is_empty()
        || select.having.is_some()
        || select.distinct
        || !select.order_by.is_empty()
        || select.limit.is_some()
    {
        return Ok(None);
    }
    let Some(kinds) = exprs.iter().map(|e| agg_kind(schema, e)).collect::<Option<Vec<_>>>()
    else {
        return Ok(None);
    };
    if kinds.is_empty() {
        return Ok(None);
    }

    let mut states: Vec<AggState> = kinds.iter().map(|k| new_state(*k)).collect();
    // Every aggregate reads a base column, so stream exactly those columns
    // straight from storage: no row is rebuilt and no other column is decoded.
    // Falls back when the child is not a bare scan (a filter, join, ...).
    let cols = aggregate_columns(&kinds);
    let slots: Vec<Option<usize>> = kinds
        .iter()
        .map(|k| k.column_index().map(|i| cols.iter().position(|&c| c == i).unwrap()))
        .collect();
    let streamed = child.for_each_projected_row(ctx, &cols, &mut |_, _, values| {
        for ((kind, state), slot) in kinds.iter().zip(states.iter_mut()).zip(&slots) {
            let value = match slot {
                Some(i) => values[*i].clone(),
                None => Value::Null,
            };
            update_value(kind, state, value)?;
        }
        Ok(())
    })?;
    if streamed == Some(true) {
        let row = states.into_iter().map(finish).collect();
        return Ok(Some(vec![row]));
    }
    // A scan can hand decoded rows straight to the accumulators, skipping
    // chunk materialization entirely.
    let fused = child.for_each_row(ctx, &mut |row| {
        for (kind, state) in kinds.iter().zip(states.iter_mut()) {
            let value = match kind.column_index() {
                Some(index) => row[index].clone(),
                None => Value::Null,
            };
            update_value(kind, state, value)?;
        }
        Ok(())
    })?;
    if !fused {
        while let Some(chunk) = child.next_chunk(ctx)? {
            for (kind, state) in kinds.iter().zip(states.iter_mut()) {
                update_state(kind, state, &chunk)?;
            }
        }
    }

    let row = states.into_iter().map(finish).collect();
    Ok(Some(vec![row]))
}

/// The distinct base columns the aggregates read, in first-seen order.
fn aggregate_columns(kinds: &[AggKind]) -> Vec<usize> {
    let mut cols = Vec::new();
    for kind in kinds {
        if let Some(i) = kind.column_index()
            && !cols.contains(&i)
        {
            cols.push(i);
        }
    }
    cols
}

#[derive(Clone, Copy)]
enum AggKind {
    /// `count(*)` when `None`, `count(col)` when `Some(column index)`.
    Count(Option<usize>),
    Sum(usize),
    Avg(usize),
    Min(usize),
    Max(usize),
}

impl AggKind {
    /// The base column this aggregate reads; `count(*)` reads none.
    fn column_index(self) -> Option<usize> {
        match self {
            AggKind::Count(None) => None,
            AggKind::Count(Some(i))
            | AggKind::Sum(i)
            | AggKind::Avg(i)
            | AggKind::Min(i)
            | AggKind::Max(i) => Some(i),
        }
    }
}

enum AggState {
    Count(i64),
    Sum(Option<Value>),
    Avg { total: f64, count: i64 },
    Min(Option<Value>),
    Max(Option<Value>),
}

fn agg_kind(schema: &Schema, expr: &Expr) -> Option<AggKind> {
    let Expr::Aggregate(func, arg, distinct) = expr else {
        return None;
    };
    if *distinct {
        return None;
    }
    let index = arg.as_deref().and_then(|a| column_index(schema, a));
    match func {
        AggFunc::Count => match arg {
            None => Some(AggKind::Count(None)),
            Some(_) => index.map(|i| AggKind::Count(Some(i))),
        },
        AggFunc::Sum => index.map(AggKind::Sum),
        AggFunc::Avg => index.map(AggKind::Avg),
        AggFunc::Min => index.map(AggKind::Min),
        AggFunc::Max => index.map(AggKind::Max),
    }
}

fn column_index(schema: &Schema, expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Column(name) => schema.resolve(None, name).ok(),
        Expr::QualifiedColumn(owner, name) => schema.resolve(Some(owner), name).ok(),
        _ => None,
    }
}

fn new_state(kind: AggKind) -> AggState {
    match kind {
        AggKind::Count(_) => AggState::Count(0),
        AggKind::Sum(_) => AggState::Sum(None),
        AggKind::Avg(_) => AggState::Avg { total: 0.0, count: 0 },
        AggKind::Min(_) => AggState::Min(None),
        AggKind::Max(_) => AggState::Max(None),
    }
}

/// Accumulates a whole chunk. Int and float columns use tight per-column
/// kernels; every other type falls back to the per-row [`update_row`] so the
/// result stays identical to [`eval_aggregate`].
fn update_state(kind: &AggKind, state: &mut AggState, chunk: &Chunk) -> Result<()> {
    match kind {
        AggKind::Count(None) => {
            let AggState::Count(n) = state else { unreachable!("count state") };
            *n += chunk.len() as i64;
            Ok(())
        }
        AggKind::Count(Some(index)) => {
            let AggState::Count(n) = state else { unreachable!("count state") };
            *n += count_present(chunk.column(*index)) as i64;
            Ok(())
        }
        AggKind::Sum(index) | AggKind::Avg(index) | AggKind::Min(index) | AggKind::Max(index) => {
            match chunk.column(*index) {
                Column::Int(values) => apply_int(kind, state, values),
                Column::Float(values) => apply_float(kind, state, values),
                _ => {
                    for i in 0..chunk.len() {
                        update_row(kind, state, chunk, i)?;
                    }
                    Ok(())
                }
            }
        }
    }
}

fn count_present(column: &Column) -> usize {
    match column {
        Column::Bool(v) => v.iter().filter(|x| x.is_some()).count(),
        Column::Int(v) => v.iter().filter(|x| x.is_some()).count(),
        Column::Float(v) => v.iter().filter(|x| x.is_some()).count(),
        Column::Str(v) => v.iter().filter(|x| x.is_some()).count(),
        Column::Date(v) => v.iter().filter(|x| x.is_some()).count(),
    }
}

fn int_overflow() -> Error {
    Error::Runtime("integer overflow".into())
}

fn apply_int(kind: &AggKind, state: &mut AggState, values: &[Option<i64>]) -> Result<()> {
    match kind {
        AggKind::Sum(_) => {
            let AggState::Sum(acc) = state else { unreachable!("sum state") };
            let mut total = match acc.take() {
                None => None,
                Some(Value::Int(x)) => Some(x),
                Some(other) => {
                    *acc = Some(other);
                    return Err(type_mismatch());
                }
            };
            for &x in values.iter().flatten() {
                total = Some(match total {
                    None => x,
                    Some(a) => a.checked_add(x).ok_or_else(int_overflow)?,
                });
            }
            *acc = total.map(Value::Int);
            Ok(())
        }
        AggKind::Avg(_) => {
            let AggState::Avg { total, count } = state else { unreachable!("avg state") };
            for &x in values.iter().flatten() {
                *total += x as f64;
                *count += 1;
            }
            Ok(())
        }
        AggKind::Min(_) | AggKind::Max(_) => {
            let is_min = matches!(kind, AggKind::Min(_));
            let best = match state {
                AggState::Min(best) | AggState::Max(best) => best,
                _ => unreachable!("min/max state"),
            };
            let mut current = match best.take() {
                None => None,
                Some(Value::Int(x)) => Some(x),
                Some(other) => {
                    *best = Some(other);
                    return Err(type_mismatch());
                }
            };
            for &x in values.iter().flatten() {
                current = Some(match current {
                    None => x,
                    Some(prev) => {
                        let take = if is_min { x < prev } else { x > prev };
                        if take { x } else { prev }
                    }
                });
            }
            *best = current.map(Value::Int);
            Ok(())
        }
        AggKind::Count(_) => unreachable!("count handled above"),
    }
}

fn apply_float(kind: &AggKind, state: &mut AggState, values: &[Option<f64>]) -> Result<()> {
    match kind {
        AggKind::Sum(_) => {
            let AggState::Sum(acc) = state else { unreachable!("sum state") };
            let mut total = match acc.take() {
                None => None,
                Some(Value::Float(x)) => Some(x),
                Some(other) => {
                    *acc = Some(other);
                    return Err(type_mismatch());
                }
            };
            for &x in values.iter().flatten() {
                total = Some(total.map_or(x, |a| a + x));
            }
            *acc = total.map(Value::Float);
            Ok(())
        }
        AggKind::Avg(_) => {
            let AggState::Avg { total, count } = state else { unreachable!("avg state") };
            for &x in values.iter().flatten() {
                *total += x;
                *count += 1;
            }
            Ok(())
        }
        AggKind::Min(_) | AggKind::Max(_) => {
            let is_min = matches!(kind, AggKind::Min(_));
            let best = match state {
                AggState::Min(best) | AggState::Max(best) => best,
                _ => unreachable!("min/max state"),
            };
            let mut current = match best.take() {
                None => None,
                Some(Value::Float(x)) => Some(x),
                Some(other) => {
                    *best = Some(other);
                    return Err(type_mismatch());
                }
            };
            for &x in values.iter().flatten() {
                current = Some(match current {
                    None => x,
                    Some(prev) => {
                        let ord = prev.partial_cmp(&x).ok_or_else(type_mismatch)?;
                        let take = if is_min {
                            ord == std::cmp::Ordering::Greater
                        } else {
                            ord == std::cmp::Ordering::Less
                        };
                        if take { x } else { prev }
                    }
                });
            }
            *best = current.map(Value::Float);
            Ok(())
        }
        AggKind::Count(_) => unreachable!("count handled above"),
    }
}

fn update_row(
    kind: &AggKind,
    state: &mut AggState,
    chunk: &Chunk,
    i: usize,
) -> Result<()> {
    match kind.column_index() {
        Some(index) => update_value(kind, state, chunk.column(index).value(i)),
        None => update_value(kind, state, Value::Null),
    }
}

/// Folds one value into its accumulator. Shared by the chunk kernels, the
/// fused scan, and the grouped path so all match [`eval_aggregate`].
fn update_value(kind: &AggKind, state: &mut AggState, value: Value) -> Result<()> {
    match kind {
        AggKind::Count(None) => {
            let AggState::Count(n) = state else { unreachable!("count state") };
            *n += 1;
        }
        AggKind::Count(Some(_)) => {
            if !matches!(value, Value::Null) {
                let AggState::Count(n) = state else { unreachable!("count state") };
                *n += 1;
            }
        }
        AggKind::Sum(_) => {
            if !matches!(value, Value::Null) {
                let AggState::Sum(acc) = state else { unreachable!("sum state") };
                *acc = Some(match acc.take() {
                    None => value,
                    // Fast paths keep the common numeric sum off `eval_binary`.
                    Some(Value::Int(a)) => match value {
                        Value::Int(b) => Value::Int(a.checked_add(b).ok_or_else(int_overflow)?),
                        other => eval_binary(BinOp::Add, Value::Int(a), other)?,
                    },
                    Some(Value::Float(a)) => match value {
                        Value::Float(b) => Value::Float(a + b),
                        other => eval_binary(BinOp::Add, Value::Float(a), other)?,
                    },
                    Some(prev) => eval_binary(BinOp::Add, prev, value)?,
                });
            }
        }
        AggKind::Avg(_) => match value {
            Value::Null => {}
            Value::Int(n) => {
                let AggState::Avg { total, count } = state else { unreachable!("avg state") };
                *total += n as f64;
                *count += 1;
            }
            Value::Float(x) => {
                let AggState::Avg { total, count } = state else { unreachable!("avg state") };
                *total += x;
                *count += 1;
            }
            _ => return Err(type_mismatch()),
        },
        AggKind::Min(_) | AggKind::Max(_) => {
            if !matches!(value, Value::Null) {
                let is_min = matches!(kind, AggKind::Min(_));
                let best = match state {
                    AggState::Min(best) | AggState::Max(best) => best,
                    _ => unreachable!("min/max state"),
                };
                *best = Some(match best.take() {
                    None => value,
                    Some(prev) => {
                        let ord = cmp_values(&prev, &value)?.ok_or_else(type_mismatch)?;
                        let take = if is_min {
                            ord == std::cmp::Ordering::Greater
                        } else {
                            ord == std::cmp::Ordering::Less
                        };
                        if take { value } else { prev }
                    }
                });
            }
        }
    }
    Ok(())
}

fn finish(state: AggState) -> Value {
    match state {
        AggState::Count(n) => Value::Int(n),
        AggState::Sum(acc) => acc.unwrap_or(Value::Null),
        AggState::Avg { total, count } => {
            if count == 0 {
                Value::Null
            } else {
                Value::Float(total / count as f64)
            }
        }
        AggState::Min(best) | AggState::Max(best) => best.unwrap_or(Value::Null),
    }
}

/// One output column of a grouped aggregate.
enum GroupProj {
    /// The first row's value of a base column within each group.
    First(usize),
    /// The next aggregate state, consumed in projection order.
    Agg,
}

/// Columnar GROUP BY for the common shape: every group key and aggregate
/// argument is a base column, every projection is such a column or a direct
/// aggregate, and there is no HAVING/DISTINCT/ORDER BY/LIMIT. Groups are
/// emitted in first-seen order, matching the row path. Returns `Ok(None)`
/// without consuming the child when the shape does not qualify.
pub(crate) fn chunk_grouped_aggregate(
    ctx: &mut ExecContext<'_>,
    schema: &Schema,
    child: &mut dyn PhysicalOperator,
    select: &SelectStmt,
    exprs: &[Expr],
) -> Result<Option<Vec<Vec<Value>>>> {
    if select.group_by.is_empty()
        || select.having.is_some()
        || select.distinct
        || !select.order_by.is_empty()
        || select.limit.is_some()
    {
        return Ok(None);
    }
    let mut group_columns = Vec::with_capacity(select.group_by.len());
    for g in &select.group_by {
        match column_index(schema, g) {
            Some(index) => group_columns.push(index),
            None => return Ok(None),
        }
    }
    let mut projection = Vec::with_capacity(exprs.len());
    let mut kinds = Vec::new();
    for e in exprs {
        if let Some(index) = column_index(schema, e) {
            projection.push(GroupProj::First(index));
        } else if let Some(kind) = agg_kind(schema, e) {
            projection.push(GroupProj::Agg);
            kinds.push(kind);
        } else {
            return Ok(None);
        }
    }

    struct Group {
        states: Vec<AggState>,
        first: Vec<Value>,
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut lookup: HashMap<Vec<u8>, usize> = HashMap::new();

    // Columns the projected push must read: group keys, aggregate arguments,
    // and any base column projected by value. Read once, in `needed` order.
    let mut needed: Vec<usize> = Vec::new();
    for &c in &group_columns {
        if !needed.contains(&c) {
            needed.push(c);
        }
    }
    for p in &projection {
        if let GroupProj::First(i) = p
            && !needed.contains(i)
        {
            needed.push(*i);
        }
    }
    for kind in &kinds {
        if let Some(i) = kind.column_index()
            && !needed.contains(&i)
        {
            needed.push(i);
        }
    }
    let slot = |c: usize| needed.iter().position(|&x| x == c).expect("column is needed");
    let group_slots: Vec<usize> = group_columns.iter().map(|&c| slot(c)).collect();
    let agg_slots: Vec<Option<usize>> = kinds.iter().map(|k| k.column_index().map(slot)).collect();
    // Reused across rows so grouping does not allocate a key per row.
    let mut key_buf: Vec<Value> = Vec::with_capacity(group_slots.len());
    let mut encoded: Vec<u8> = Vec::new();

    // A bare scan streams exactly the needed columns without materializing
    // chunks; anything else (a filter, join, ...) falls back below.
    let streamed = child
        .for_each_projected_row(ctx, &needed, &mut |_, _, values| {
            key_buf.clear();
            key_buf.extend(group_slots.iter().map(|&s| values[s].clone()));
            crate::storage::codec::encode_row_into(&key_buf, &mut encoded);
            let group = match lookup.get(&encoded) {
                Some(&g) => g,
                None => {
                    let g = groups.len();
                    lookup.insert(encoded.clone(), g);
                    groups.push(Group {
                        states: kinds.iter().map(|k| new_state(*k)).collect(),
                        first: values.to_vec(),
                    });
                    g
                }
            };
            for ((kind, state), slot) in
                kinds.iter().zip(groups[group].states.iter_mut()).zip(&agg_slots)
            {
                let value = match slot {
                    Some(s) => values[*s].clone(),
                    None => Value::Null,
                };
                update_value(kind, state, value)?;
            }
            Ok(())
        })?
        .unwrap_or(false);

    if !streamed {
        while let Some(chunk) = child.next_chunk(ctx)? {
            for i in 0..chunk.len() {
                key_buf.clear();
                key_buf.extend(group_columns.iter().map(|&c| chunk.column(c).value(i)));
                crate::storage::codec::encode_row_into(&key_buf, &mut encoded);
                let group = match lookup.get(&encoded) {
                    Some(&g) => g,
                    None => {
                        let g = groups.len();
                        lookup.insert(encoded.clone(), g);
                        groups.push(Group {
                            states: kinds.iter().map(|k| new_state(*k)).collect(),
                            first: needed.iter().map(|&c| chunk.column(c).value(i)).collect(),
                        });
                        g
                    }
                };
                for (kind, state) in kinds.iter().zip(groups[group].states.iter_mut()) {
                    update_row(kind, state, &chunk, i)?;
                }
            }
        }
    }

    let mut out = Vec::with_capacity(groups.len());
    for group in groups {
        let mut states = group.states.into_iter();
        let mut row = Vec::with_capacity(projection.len());
        for p in &projection {
            match p {
                GroupProj::First(index) => row.push(group.first[slot(*index)].clone()),
                GroupProj::Agg => {
                    row.push(finish(states.next().expect("one state per aggregate")))
                }
            }
        }
        out.push(row);
    }
    Ok(Some(out))
}
