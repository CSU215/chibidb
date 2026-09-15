use std::collections::HashMap;

use crate::sql::ast::{AggFunc, BinOp, Expr, Limit, SelectItem};
use crate::catalog::{ColumnDesc, Schema};
use crate::config::ExecutionMode;
use crate::txn::trx::TrxState;
use crate::value::{DataType, Value};
use crate::{Database, Error, Result};

use super::chunk::{CHUNK_ROWS, Chunk};
use super::eval::{cmp_values, eval, eval_binary, eval_const, type_mismatch, EvalCtx};
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

fn int_overflow() -> Error {
    Error::Runtime("integer overflow".into())
}

/// Folds one value into its accumulator. Shared by the fused scan and the row
/// path so both match [`eval_aggregate`].
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

/// The output column name for the `index`-th aggregate of an [`Aggregate`].
pub(crate) fn agg_column(index: usize) -> String {
    format!("#agg{index}")
}

/// The distinct aggregate expressions in `exprs`, in first-seen order.
pub(crate) fn extract_aggregates(exprs: &[&Expr]) -> Vec<Expr> {
    let mut out = Vec::new();
    for e in exprs {
        collect_aggregates(e, &mut out);
    }
    out
}

fn collect_aggregates(e: &Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::Aggregate(..) => {
            if !out.contains(e) {
                out.push(e.clone());
            }
        }
        Expr::Unary(_, a) => collect_aggregates(a, out),
        Expr::Binary(_, l, r) => {
            collect_aggregates(l, out);
            collect_aggregates(r, out);
        }
        Expr::IsNull(a, _) => collect_aggregates(a, out),
        Expr::Like { expr, pattern, .. } => {
            collect_aggregates(expr, out);
            collect_aggregates(pattern, out);
        }
        Expr::Function(_, args) => {
            for a in args {
                collect_aggregates(a, out);
            }
        }
        _ => {}
    }
}

/// Replaces every aggregate subexpression in `e` with a reference to its
/// `#aggN` output column of [`Aggregate`].
pub(crate) fn rewrite_aggregates(e: &Expr, aggregates: &[Expr]) -> Expr {
    if let Some(i) = aggregates.iter().position(|a| a == e) {
        return Expr::Column(agg_column(i));
    }
    match e {
        Expr::Unary(op, a) => Expr::Unary(*op, Box::new(rewrite_aggregates(a, aggregates))),
        Expr::Binary(op, l, r) => Expr::Binary(
            *op,
            Box::new(rewrite_aggregates(l, aggregates)),
            Box::new(rewrite_aggregates(r, aggregates)),
        ),
        Expr::IsNull(a, n) => Expr::IsNull(Box::new(rewrite_aggregates(a, aggregates)), *n),
        Expr::Like { expr, pattern, negated, escape } => Expr::Like {
            expr: Box::new(rewrite_aggregates(expr, aggregates)),
            pattern: Box::new(rewrite_aggregates(pattern, aggregates)),
            negated: *negated,
            escape: *escape,
        },
        Expr::Function(name, args) => Expr::Function(
            name.clone(),
            args.iter().map(|a| rewrite_aggregates(a, aggregates)).collect(),
        ),
        other => other.clone(),
    }
}

/// The input column indices referenced by `exprs`, in first-seen order, so the
/// fused scan knows which columns to provide.
pub(crate) fn referenced_columns(schema: &Schema, exprs: &[&Expr]) -> Vec<usize> {
    let mut out = Vec::new();
    for e in exprs {
        collect_columns(schema, e, &mut out);
    }
    out
}

fn collect_columns(schema: &Schema, e: &Expr, out: &mut Vec<usize>) {
    match e {
        Expr::Column(_) | Expr::QualifiedColumn(..) => {
            if let Some(i) = column_index(schema, e)
                && !out.contains(&i)
            {
                out.push(i);
            }
        }
        Expr::Unary(_, a) => collect_columns(schema, a, out),
        Expr::Binary(_, l, r) => {
            collect_columns(schema, l, out);
            collect_columns(schema, r, out);
        }
        Expr::IsNull(a, _) => collect_columns(schema, a, out),
        Expr::Like { expr, pattern, .. } => {
            collect_columns(schema, expr, out);
            collect_columns(schema, pattern, out);
        }
        Expr::Function(_, args) => {
            for a in args {
                collect_columns(schema, a, out);
            }
        }
        Expr::Aggregate(_, Some(arg), _) => collect_columns(schema, arg, out),
        _ => {}
    }
}

/// Standard physical GROUP BY: one row per group, made of the group's first
/// input row (all columns) followed by one column per aggregate (`#agg0`, ...).
/// HAVING, ORDER BY, projection, DISTINCT and LIMIT are separate operators
/// above it. The fused scan path (streaming only the needed columns into the
/// accumulators) is preserved for base-column group keys and arguments.
pub struct Aggregate {
    child: Box<dyn PhysicalOperator>,
    input_schema: Schema,
    schema: Schema,
    group_by: Vec<Expr>,
    aggregates: Vec<Expr>,
    /// `Some` when every aggregate is a direct base-column aggregation.
    kinds: Option<Vec<AggKind>>,
    /// Input columns the fused read must supply.
    needed: Vec<usize>,
    rows: Vec<Vec<Value>>,
    pos: usize,
}

impl Aggregate {
    pub fn new(
        child: Box<dyn PhysicalOperator>,
        input_schema: Schema,
        group_by: Vec<Expr>,
        aggregates: Vec<Expr>,
        needed: Vec<usize>,
    ) -> Self {
        let kinds = aggregates
            .iter()
            .map(|e| agg_kind(&input_schema, e))
            .collect::<Option<Vec<_>>>();
        let mut schema = input_schema.clone();
        for (i, _) in aggregates.iter().enumerate() {
            schema.columns.push(ColumnDesc::plain(None, agg_column(i), DataType::Text));
        }
        Self { child, input_schema, schema, group_by, aggregates, kinds, needed, rows: Vec::new(), pos: 0 }
    }

    /// Groups materialized rows in first-seen order, matching the old path.
    fn group_rows(
        &self,
        ctx: &mut ExecContext<'_>,
        rows: Vec<Vec<Value>>,
    ) -> Result<Vec<Vec<Value>>> {
        let width = self.input_schema.columns.len();
        let mut groups: Vec<(Vec<Value>, Vec<Vec<Value>>)> = Vec::new();
        if self.group_by.is_empty() {
            groups.push((Vec::new(), rows));
        } else {
            for row in rows {
                let mut eval_ctx = EvalCtx::row(&self.input_schema, &row);
                eval_ctx.parent = ctx.outer;
                let mut key = Vec::with_capacity(self.group_by.len());
                for g in &self.group_by {
                    key.push(eval_bound(ctx.db, ctx.trx, g, Some(&eval_ctx))?);
                }
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, group)) => group.push(row),
                    None => groups.push((key, vec![row])),
                }
            }
        }
        let mut out = Vec::with_capacity(groups.len());
        for (_, group) in groups {
            let mut row = group
                .first()
                .cloned()
                .unwrap_or_else(|| vec![Value::Null; width]);
            for agg in &self.aggregates {
                let Expr::Aggregate(func, arg, distinct) = agg else {
                    unreachable!("aggregate list holds aggregates")
                };
                row.push(eval_aggregate(
                    *func,
                    arg.as_deref(),
                    *distinct,
                    &self.input_schema,
                    &group,
                )?);
            }
            out.push(row);
        }
        Ok(out)
    }

    /// Streams the needed base columns straight into the accumulators, without
    /// materializing rows. Returns `false` when the shape or child does not
    /// qualify, so the caller uses [`Aggregate::group_rows`].
    fn open_fused(&mut self, ctx: &mut ExecContext<'_>, kinds: &[AggKind]) -> Result<bool> {
        let Some(group_columns) = self
            .group_by
            .iter()
            .map(|g| column_index(&self.input_schema, g))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(false);
        };
        let slot = |c: usize| self.needed.iter().position(|&x| x == c);
        let Some(group_slots) = group_columns.iter().map(|&c| slot(c)).collect::<Option<Vec<_>>>()
        else {
            return Ok(false);
        };
        let mut agg_slots = Vec::with_capacity(kinds.len());
        for kind in kinds {
            match kind.column_index() {
                Some(c) => match slot(c) {
                    Some(s) => agg_slots.push(Some(s)),
                    None => return Ok(false),
                },
                None => agg_slots.push(None),
            }
        }

        struct Group {
            states: Vec<AggState>,
            first: Vec<Value>,
        }
        let mut groups: Vec<Group> = Vec::new();
        let mut lookup: HashMap<Vec<u8>, usize> = HashMap::new();
        let mut key_buf: Vec<Value> = Vec::with_capacity(group_slots.len());
        let mut encoded: Vec<u8> = Vec::new();
        // The row path groups by `Value` equality, where -0.0 == 0.0; normalise
        // the signed zero so the encoded key agrees.
        let normalize = |mut value: Value| {
            if let Value::Float(f) = &value
                && *f == 0.0
            {
                value = Value::Float(0.0);
            }
            value
        };
        let needed = self.needed.clone();
        let streamed = self.child.for_each_projected_row(ctx, &needed, &mut |_, _, values| {
            key_buf.clear();
            key_buf.extend(group_slots.iter().map(|&s| normalize(values[s].clone())));
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
                    Some(i) => values[*i].clone(),
                    None => Value::Null,
                };
                update_value(kind, state, value)?;
            }
            Ok(())
        })?;
        if streamed != Some(true) {
            return Ok(false);
        }
        if self.group_by.is_empty() && groups.is_empty() {
            groups.push(Group {
                states: kinds.iter().map(|k| new_state(*k)).collect(),
                first: vec![Value::Null; needed.len()],
            });
        }
        let width = self.input_schema.columns.len();
        let mut out = Vec::with_capacity(groups.len());
        for group in groups {
            let mut row = vec![Value::Null; width];
            for (i, &index) in needed.iter().enumerate() {
                if i < group.first.len() {
                    row[index] = group.first[i].clone();
                }
            }
            for state in group.states {
                row.push(finish(state));
            }
            out.push(row);
        }
        self.rows = out;
        Ok(true)
    }
}

impl PhysicalOperator for Aggregate {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn aggregate_parts(&self) -> Option<(&Schema, &[Expr])> {
        Some((&self.input_schema, &self.aggregates))
    }

    fn label(&self) -> String {
        format!("Aggregate groups={} aggs={}", self.group_by.len(), self.aggregates.len())
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.child.open(ctx)?;
        let mut fused = false;
        if ctx.db.config().execution.mode == ExecutionMode::Chunk
            && let Some(kinds) = self.kinds.clone()
        {
            fused = self.open_fused(ctx, &kinds)?;
        }
        if !fused {
            let mut rows = Vec::new();
            while let Some(row) = self.child.next(ctx)? {
                rows.push(row);
            }
            self.rows = self.group_rows(ctx, rows)?;
        }
        self.child.close()?;
        self.pos = 0;
        Ok(())
    }

    fn next(&mut self, _ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        if self.pos >= self.rows.len() {
            return Ok(None);
        }
        let row = self.rows[self.pos].clone();
        self.pos += 1;
        Ok(Some(row))
    }

    fn next_chunk(&mut self, _ctx: &mut ExecContext<'_>) -> Result<Option<Chunk>> {
        if self.pos >= self.rows.len() {
            return Ok(None);
        }
        let end = (self.pos + CHUNK_ROWS).min(self.rows.len());
        let batch = Chunk::from_rows(&self.rows[self.pos..end])?;
        self.pos = end;
        Ok(Some(batch))
    }

    fn chunk_native(&self) -> bool {
        true
    }

    fn close(&mut self) -> Result<()> {
        self.rows.clear();
        self.pos = 0;
        Ok(())
    }
}
