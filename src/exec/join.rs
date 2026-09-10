use crate::ast::{DataType, JoinKind, SelectStmt, Stmt, TableRef};
use crate::catalog::Schema;
use crate::result::ResultSet;
use crate::trx::TrxState;
use crate::value::Value;
use crate::{Database, Error, Result};

use super::eval::EvalCtx;
use super::subquery::eval_predicate_bound;
use super::{decode_visible, execute_select};

/// Resolves a FROM reference: a real table (MVCC-visible rows) or a view
/// (executes its stored select; views over views recurse).
fn from_source(
    db: &mut Database,
    trx: &mut TrxState,
    tref: &TableRef,
) -> Result<(Vec<crate::catalog::ColumnDesc>, Vec<Vec<Value>>)> {
    if let Ok(table) = db.catalog().table(&tref.name) {
        let cols = table.schema.columns.clone();
        let records = db.store_scan_raw(&tref.name)?;
        return Ok((cols, decode_visible(records, trx)?));
    }
    let sql = db
        .catalog()
        .view(&tref.name)
        .ok_or_else(|| Error::Runtime(format!("no such table: {}", tref.name)))?
        .clone();
    let stmts = crate::parser::parse(&sql)?;
    let Some(Stmt::Select(sel)) = stmts.into_iter().next() else {
        return Err(Error::Runtime(format!("corrupt view definition: {}", tref.name)));
    };
    match execute_select(db, trx, &sel, None)? {
        ResultSet::Rows { columns, rows } => Ok((
            columns
                .into_iter()
                .map(|name| crate::catalog::ColumnDesc {
                    owner: None,
                    name,
                    // view columns carry no storage type; unused in queries
                    dtype: DataType::Text,
                })
                .collect(),
            rows,
        )),
        ResultSet::Message(_) => {
            Err(Error::Runtime(format!("corrupt view definition: {}", tref.name)))
        }
    }
}

/// Nested-loop join over all FROM tables (comma list and JOIN..ON alike),
/// returning the combined schema and the joined rows.
pub(crate) fn nested_loop(
    db: &mut Database,
    trx: &mut TrxState,
    s: &SelectStmt,
    outer: Option<&EvalCtx>,
) -> Result<(Schema, Vec<Vec<Value>>)> {
    let mut schema = Schema::default();
    let mut rows: Vec<Vec<Value>> = vec![vec![]];
    for (i, tref) in s.from.iter().enumerate() {
        let owner = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
        let (columns, visible) = from_source(db, trx, tref)?;
        let right_cols = columns.len();
        for col in columns {
            schema.columns.push(crate::catalog::ColumnDesc {
                owner: Some(owner.clone()),
                name: col.name,
                dtype: col.dtype,
            });
        }
        let kind = s.joins.get(i).copied().unwrap_or(JoinKind::Cross);
        let cond = if i >= 1 { s.on.get(i - 1) } else { None };
        let mut combined = Vec::with_capacity(rows.len() * visible.len().max(1));
        if i >= 1 && kind == JoinKind::Left {
            let Some(cond) = cond else {
                return Err(Error::Runtime("left join requires an on clause".into()));
            };
            for left in rows {
                let mut matched = false;
                for right in &visible {
                    let mut row = left.clone();
                    row.extend(right.iter().cloned());
                    if eval_predicate_bound(db, trx, cond, &schema, &row, outer)? {
                        combined.push(row);
                        matched = true;
                    }
                }
                if !matched {
                    let mut row = left;
                    row.extend(vec![Value::Null; right_cols]);
                    combined.push(row);
                }
            }
        } else {
            for left in rows {
                for right in &visible {
                    let mut row = left.clone();
                    row.extend(right.iter().cloned());
                    combined.push(row);
                }
            }
            if i >= 1
                && let Some(cond) = cond {
                    let mut kept = Vec::with_capacity(combined.len());
                    for row in combined {
                        if eval_predicate_bound(db, trx, cond, &schema, &row, outer)? {
                            kept.push(row);
                        }
                    }
                    combined = kept;
                }
        }
        rows = combined;
    }
    Ok((schema, rows))
}
