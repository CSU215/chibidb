use crate::ast::{BinOp, DataType, Expr, ExplainStmt, SelectStmt, Stmt};
use crate::index::{encode_key, BTree, Bound};
use crate::result::ResultSet;
use crate::storage::Rid;
use crate::trx::TrxState;
use crate::value::Value;
use crate::{Database, Error, Result};

use super::coerce;
use super::decode_visible;
use super::eval::{eval_const, expr_has_column};

pub(crate) fn execute_explain(db: &mut Database, e: &ExplainStmt) -> Result<ResultSet> {
    match &*e.stmt {
        Stmt::Select(s) => Ok(ResultSet::Message(plan_select(db, s)?)),
        _ => Err(Error::Runtime("explain supports select only".into())),
    }
}

fn plan_select(db: &mut Database, s: &SelectStmt) -> Result<String> {
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
        Some(sarg) => Ok(format!(
            "IndexScan(index={}, table={}, where {} {}) -> Filter -> Project",
            sarg.index, s.from[0].name, sarg.column, sarg.op
        )),
        None => Ok(format!("FullScan(table={}) -> Filter -> Project", s.from[0].name)),
    }
}

struct Sargable {
    index: String,
    column: String,
    dtype: DataType,
    op: BinOp,
    lit: Expr,
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

/// Rule-based access path choice: an equality or range predicate over an
/// indexed column (even buried in an AND chain) uses the index.
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
    let schema = &db.catalog().table(table)?.schema;
    for conj in split_conjuncts(sel) {
        let (col_expr, op, lit) = match conj {
            Expr::Binary(op @ (BinOp::Eq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge), l, r) => {
                if matches!(**l, Expr::Column(_)) && !expr_has_column(r) {
                    ((**l).clone(), *op, (**r).clone())
                } else if matches!(**r, Expr::Column(_)) && !expr_has_column(l) {
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
        let ix = db
            .catalog()
            .indexes_for(table)
            .into_iter()
            .find(|ix| ix.column == cname);
        if let Some(ix) = ix {
            return Ok(Some(Sargable {
                index: ix.name.clone(),
                column: cname,
                dtype: schema.columns[col_idx].dtype,
                op,
                lit,
            }));
        }
    }
    Ok(None)
}

/// If the selection is sargable, scans the index and returns the visible
/// rows for the referenced table; otherwise returns `None`.
pub(crate) fn index_scan_source(
    db: &mut Database,
    trx: &mut TrxState,
    table: &str,
    selection: Option<&Expr>,
) -> Result<Option<Vec<Vec<Value>>>> {
    let Some(sarg) = find_sargable(db, table, selection)? else {
        return Ok(None);
    };
    let lit_val = eval_const(&sarg.lit)?;
    let coerced = coerce(lit_val, sarg.dtype, &sarg.column)?;
    let key = encode_key(&coerced)?;
    let ix_file = db
        .catalog()
        .indexes_for(table)
        .into_iter()
        .find(|ix| ix.name == sarg.index)
        .map(|ix| ix.store.file)
        .expect("sargable index exists");
    let btree = BTree::at(ix_file);
    let rids: Vec<Rid> = match sarg.op {
        BinOp::Eq => btree.search(&mut db.pool, &key)?,
        BinOp::Lt => scan_rids(&btree, &mut db.pool, Bound::Unbounded, Bound::Excluded(&key))?,
        BinOp::Le => scan_rids(&btree, &mut db.pool, Bound::Unbounded, Bound::Included(&key))?,
        BinOp::Gt => scan_rids(&btree, &mut db.pool, Bound::Excluded(&key), Bound::Unbounded)?,
        BinOp::Ge => scan_rids(&btree, &mut db.pool, Bound::Included(&key), Bound::Unbounded)?,
        _ => unreachable!("sargable ops are restricted"),
    };
    Ok(Some(decode_visible(db.store_get_records(table, &rids)?, trx)?))
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
