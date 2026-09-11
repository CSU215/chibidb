use crate::ast::{BinOp, DataType, Expr, ExplainStmt, SelectStmt, Stmt};
use crate::index::{encode_key, BTree, Bound};
use crate::result::ResultSet;
use crate::storage::Rid;
use crate::{Database, Error, Result};

use super::coerce;
use super::eval::{eval_const, expr_has_column, expr_has_subquery};

pub(crate) fn execute_explain(db: &mut Database, e: &ExplainStmt) -> Result<ResultSet> {
    match &*e.stmt {
        Stmt::Select(s) => Ok(ResultSet::Message(plan_select(db, s)?)),
        _ => Err(Error::Runtime("explain supports select only".into())),
    }
}

pub(crate) fn plan_select(db: &mut Database, s: &SelectStmt) -> Result<String> {
    if s.from.is_empty() {
        return Ok("ConstantSelect -> Project".into());
    }
    if s.from.len() > 1 {
        return Ok(format!(
            "NestedLoopJoin(tables={}) -> Filter -> Project",
            s.from.len()
        ));
    }
    match find_sargable(db, &s.from[0].name, s.selection.as_ref())? {
        Some(sarg) => {
            let kind = if order_by_matches(&sarg.column, &s.order_by) {
                "OrderedIndexScan"
            } else {
                "IndexScan"
            };
            Ok(format!(
                "{kind}(index={}, table={}, {}) -> Filter -> Project",
                sarg.index,
                s.from[0].name,
                describe_sarg(&sarg)
            ))
        }
        None => Ok(format!("FullScan(table={}) -> Filter -> Project", s.from[0].name)),
    }
}

/// True when ORDER BY is a single ascending key on `column`, so an index
/// scan over that column already yields the requested order.
pub(crate) fn order_by_matches(column: &str, order_by: &[(Expr, bool)]) -> bool {
    let [(e, desc)] = order_by else {
        return false;
    };
    if *desc {
        return false;
    }
    match e {
        Expr::Column(n) | Expr::QualifiedColumn(_, n) => n == column,
        _ => false,
    }
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
    db: &mut Database,
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
    let schema = &db.catalog().table(table)?.schema;
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
        let Some(ix) = db
            .catalog()
            .indexes_for(table)
            .into_iter()
            .find(|ix| ix.column == cname)
        else {
            continue;
        };
        let dtype = schema.columns[col_idx].dtype;
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
    pub heap_file: crate::storage::FileId,
    pub column: String,
    pub rids: Vec<Rid>,
}

pub(crate) fn plan_index_scan(
    db: &mut Database,
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
            btree.search(&mut db.pool, &key)?
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
            scan_rids(&btree, &mut db.pool, start, end)?
        }
    };
    let heap_file = db.catalog().table(table)?.heap.file;
    Ok(Some(IndexScanRids { heap_file, column: sarg.column, rids }))
}

fn scan_rids(
    btree: &BTree,
    pool: &mut crate::storage::BufferPool,
    start: Bound,
    end: Bound,
) -> Result<Vec<Rid>> {
    Ok(btree
        .scan_range(pool, start, end)?
        .into_iter()
        .map(|(_, rid)| rid)
        .collect())
}
