//! Access-path selection: which index (if any) satisfies a sargable predicate,
//! and the ORDER BY column an in-order index scan can supply instead of a sort.

use crate::sql::ast::{BinOp, Expr, SelectItem};
use crate::index::{encode_key, BTree, Bound};
use crate::storage::Rid;
use crate::value::DataType;
use crate::{Database, Result};

use crate::exec::coerce;
use crate::exec::eval::{eval_const, expr_has_column, expr_has_subquery};

/// The plain column of a single ascending ORDER BY key, after resolving SELECT
/// aliases. `SELECT b AS id ... ORDER BY id` therefore resolves to `b`, so an
/// index on the base column `id` is not mistaken for satisfying the order.
pub(crate) fn resolved_order_column(
    items: &[SelectItem],
    order_by: &[(Expr, bool)],
) -> Option<String> {
    let [(e, desc)] = order_by else {
        return None;
    };
    if *desc {
        return None;
    }
    match resolve_alias(items, e) {
        Expr::Column(n) => Some(n.clone()),
        Expr::QualifiedColumn(_, n) => Some(n.clone()),
        _ => None,
    }
}

/// Replaces an unqualified column reference that names a SELECT alias with the
/// aliased expression.
fn resolve_alias<'a>(items: &'a [SelectItem], expr: &'a Expr) -> &'a Expr {
    if let Expr::Column(name) = expr {
        for item in items {
            if let SelectItem::Aliased(inner, alias) = item
                && alias == name
            {
                return inner;
            }
        }
    }
    expr
}

fn describe_sarg(s: &Sargable) -> String {
    match &s.kind {
        SargKind::Eq(lit) => format!("where {} = {lit}", s.column),
        SargKind::Range { lower, upper } => {
            let mut parts = Vec::new();
            if let Some((inclusive, lit)) = lower {
                parts.push(format!("{} {} {lit}", s.column, if *inclusive { ">=" } else { ">" }));
            }
            if let Some((inclusive, lit)) = upper {
                parts.push(format!("{} {} {lit}", s.column, if *inclusive { "<=" } else { "<" }));
            }
            format!("where {}", parts.join(" and "))
        }
    }
}

enum SargKind {
    Eq(Expr),
    /// Bounds are `(inclusive, literal)`; either side may be absent.
    Range { lower: Option<(bool, Expr)>, upper: Option<(bool, Expr)> },
}

struct Sargable {
    index: String,
    column: String,
    dtype: DataType,
    kind: SargKind,
}

fn split_conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Binary(BinOp::And, l, r) => {
            let mut out = split_conjuncts(l);
            out.extend(split_conjuncts(r));
            out
        }
        other => vec![other],
    }
}

fn flip_cmp(op: BinOp) -> Option<BinOp> {
    match op {
        BinOp::Eq => Some(BinOp::Eq),
        BinOp::Lt => Some(BinOp::Gt),
        BinOp::Le => Some(BinOp::Ge),
        BinOp::Gt => Some(BinOp::Lt),
        BinOp::Ge => Some(BinOp::Le),
        _ => None,
    }
}

/// Rule-based access path choice: equality or range predicates over an
/// indexed column (even buried in an AND chain) use the index. An equality
/// wins outright; otherwise all `>`/`>=`/`<`/`<=` conjuncts on one indexed
/// column are combined into a single bounded range scan.
fn find_sargable(
    db: &Database,
    table: &str,
    selection: Option<&Expr>,
) -> Result<Option<Sargable>> {
    let Some(sel) = selection else {
        return Ok(None);
    };
    // views have no indexes, so no access path choice applies
    if db.catalog().view(table).is_some() {
        return Ok(None);
    }
    struct Cand {
        column: String,
        index: String,
        dtype: DataType,
        op: BinOp,
        lit: Expr,
    }
    let catalog = db.catalog();
    let schema = &catalog.table(table)?.schema;
    let mut cands: Vec<Cand> = Vec::new();
    for conj in split_conjuncts(sel) {
        let (col_expr, op, lit) = match conj {
            Expr::Binary(op @ (BinOp::Eq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge), l, r) => {
                if matches!(**l, Expr::Column(_)) && !expr_has_column(r) && !expr_has_subquery(r) {
                    ((**l).clone(), *op, (**r).clone())
                } else if matches!(**r, Expr::Column(_))
                    && !expr_has_column(l)
                    && !expr_has_subquery(l)
                {
                    match flip_cmp(*op) {
                        Some(flip) => ((**r).clone(), flip, (**l).clone()),
                        None => continue,
                    }
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        let Expr::Column(cname) = col_expr else {
            continue;
        };
        let Some(col_idx) = schema.index_of(&cname) else {
            continue;
        };
        let Some(ix) = catalog.indexes_for(table).into_iter().find(|ix| ix.column == cname)
        else {
            continue;
        };
        let dtype = schema.columns[col_idx].dtype;
        // A literal that cannot be losslessly coerced to the column type must
        // not select an index: the row path compares it with `cmp_values`
        // (e.g. `int_col = 1.0`), so reject the candidate and let the full
        // scan handle it instead of erroring inside `literal_key`.
        if literal_key(&lit, dtype, &cname).is_err() {
            continue;
        }
        if op == BinOp::Eq {
            return Ok(Some(Sargable {
                index: ix.name.clone(),
                column: cname,
                dtype,
                kind: SargKind::Eq(lit),
            }));
        }
        cands.push(Cand { column: cname, index: ix.name.clone(), dtype, op, lit });
    }
    let Some(first) = cands.first() else {
        return Ok(None);
    };
    let column = first.column.clone();
    let index = first.index.clone();
    let dtype = first.dtype;
    let mut lower = None;
    let mut upper = None;
    for c in &cands {
        if c.column != column {
            continue;
        }
        match c.op {
            BinOp::Gt => lower = Some((false, c.lit.clone())),
            BinOp::Ge => lower = Some((true, c.lit.clone())),
            BinOp::Lt => upper = Some((false, c.lit.clone())),
            BinOp::Le => upper = Some((true, c.lit.clone())),
            _ => {}
        }
    }
    Ok(Some(Sargable { index, column, dtype, kind: SargKind::Range { lower, upper } }))
}

fn literal_key(lit: &Expr, dtype: DataType, column: &str) -> Result<Vec<u8>> {
    let v = eval_const(lit)?;
    let coerced = coerce(v, dtype, column)?;
    encode_key(&coerced)
}

fn bound<'a>(key: Option<&'a Vec<u8>>, inclusive: bool) -> Bound<'a> {
    match key {
        Some(k) if inclusive => Bound::Included(k),
        Some(k) => Bound::Excluded(k),
        None => Bound::Unbounded,
    }
}

/// Index-derived row ids for a sargable selection, if one applies. The
/// `IndexScan` operator consumes this.
pub(crate) struct IndexScanRids {
    pub column: String,
    /// Index name and a human-readable predicate, for EXPLAIN / visualisation.
    pub index: String,
    pub predicate: String,
    pub rids: Vec<Rid>,
}

pub(crate) fn plan_index_scan(
    db: &Database,
    table: &str,
    selection: Option<&Expr>,
) -> Result<Option<IndexScanRids>> {
    let Some(sarg) = find_sargable(db, table, selection)? else {
        return Ok(None);
    };
    let ix_file = db
        .catalog()
        .indexes_for(table)
        .into_iter()
        .find(|ix| ix.name == sarg.index)
        .map(|ix| ix.store.file)
        .expect("sargable index exists");
    let btree = BTree::at(ix_file);
    let rids: Vec<Rid> = match &sarg.kind {
        SargKind::Eq(lit) => {
            let key = literal_key(lit, sarg.dtype, &sarg.column)?;
            btree.search(&db.pool, &key)?
        }
        SargKind::Range { lower, upper } => {
            let lower_key = lower
                .as_ref()
                .map(|(_, l)| literal_key(l, sarg.dtype, &sarg.column))
                .transpose()?;
            let upper_key = upper
                .as_ref()
                .map(|(_, l)| literal_key(l, sarg.dtype, &sarg.column))
                .transpose()?;
            let start = bound(lower_key.as_ref(), lower.as_ref().is_some_and(|(i, _)| *i));
            let end = bound(upper_key.as_ref(), upper.as_ref().is_some_and(|(i, _)| *i));
            scan_rids(&btree, &db.pool, start, end)?
        }
    };
    let predicate = describe_sarg(&sarg);
    Ok(Some(IndexScanRids {
        column: sarg.column,
        index: sarg.index,
        predicate,
        rids,
    }))
}

fn scan_rids(
    btree: &BTree,
    pool: &crate::storage::BufferPool,
    start: Bound,
    end: Bound,
) -> Result<Vec<Rid>> {
    Ok(btree
        .scan_range(pool, start, end)?
        .into_iter()
        .map(|(_, rid)| rid)
        .collect())
}

/// The index file backing an in-order scan of `column`, used to satisfy an
/// `ORDER BY column` even when there is no WHERE clause to make it sargable.
pub(crate) fn ordered_index_file(
    db: &Database,
    table: &str,
    column: &str,
) -> Result<Option<crate::storage::page::FileId>> {
    let catalog = db.catalog();
    if catalog.view(table).is_some() {
        return Ok(None);
    }
    Ok(catalog
        .indexes_for(table)
        .into_iter()
        .find(|ix| ix.column == column)
        .map(|ix| ix.store.file))
}
