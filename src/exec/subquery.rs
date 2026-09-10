use crate::ast::{BinOp, Expr, SelectItem, SelectStmt, UnOp};
use crate::result::ResultSet;
use crate::trx::TrxState;
use crate::value::Value;
use crate::{Database, Error, Result};

use super::execute_select;

/// Materializes every uncorrelated subquery in a select into literal
/// values before row evaluation, so `eval` stays context-free. Subqueries
/// inside subqueries recurse naturally (each `execute_select` lifts again).
pub(crate) fn lift_subqueries(
    db: &mut Database,
    trx: &mut TrxState,
    s: &SelectStmt,
) -> Result<SelectStmt> {
    let mut items = Vec::with_capacity(s.items.len());
    for it in &s.items {
        items.push(match it {
            SelectItem::Star => SelectItem::Star,
            SelectItem::Expr(e) => SelectItem::Expr(lift_expr(db, trx, e)?),
            SelectItem::Aliased(e, a) => SelectItem::Aliased(lift_expr(db, trx, e)?, a.clone()),
        });
    }
    let mut on = Vec::with_capacity(s.on.len());
    for e in &s.on {
        on.push(lift_expr(db, trx, e)?);
    }
    let selection = match &s.selection {
        Some(e) => Some(lift_expr(db, trx, e)?),
        None => None,
    };
    let mut group_by = Vec::with_capacity(s.group_by.len());
    for e in &s.group_by {
        group_by.push(lift_expr(db, trx, e)?);
    }
    let having = match &s.having {
        Some(e) => Some(lift_expr(db, trx, e)?),
        None => None,
    };
    let mut order_by = Vec::with_capacity(s.order_by.len());
    for (e, desc) in &s.order_by {
        order_by.push((lift_expr(db, trx, e)?, *desc));
    }
    Ok(SelectStmt { distinct: s.distinct, items, from: s.from.clone(), joins: s.joins.clone(), on, selection, group_by, having, order_by, limit: s.limit.clone() })
}

fn lift_expr(db: &mut Database, trx: &mut TrxState, e: &Expr) -> Result<Expr> {
    match e {
        Expr::InSubquery { expr, sub, negated } => {
            let inner = lift_expr(db, trx, expr)?;
            let (cols, rows) = run_subquery(db, trx, sub)?;
            if cols.len() != 1 {
                return Err(Error::Runtime(
                    "subquery in IN must return a single column".into(),
                ));
            }
            let mut acc: Option<Expr> = None;
            for row in rows {
                let eq = Expr::Binary(
                    BinOp::Eq,
                    Box::new(inner.clone()),
                    Box::new(Expr::Value(row.into_iter().next().expect("single column"))),
                );
                acc = Some(match acc {
                    None => eq,
                    Some(prev) => Expr::Binary(BinOp::Or, Box::new(prev), Box::new(eq)),
                });
            }
            // empty set: IN is FALSE, NOT IN is TRUE (via NOT FALSE)
            let folded = acc.unwrap_or(Expr::Value(Value::Bool(false)));
            Ok(if *negated { Expr::Unary(UnOp::Not, Box::new(folded)) } else { folded })
        }
        Expr::Exists { sub } => {
            let (_, rows) = run_subquery(db, trx, sub)?;
            Ok(Expr::Value(Value::Bool(!rows.is_empty())))
        }
        Expr::ScalarSubquery(sub) => {
            let (cols, rows) = run_subquery(db, trx, sub)?;
            if cols.len() != 1 {
                return Err(Error::Runtime(
                    "scalar subquery must return a single column".into(),
                ));
            }
            match rows.len() {
                0 => Ok(Expr::Value(Value::Null)),
                1 => Ok(Expr::Value(rows.into_iter().next().expect("one row").into_iter().next().expect("one column"))),
                _ => Err(Error::Runtime(
                    "scalar subquery returned more than one row".into(),
                )),
            }
        }
        Expr::Unary(op, inner) => Ok(Expr::Unary(*op, Box::new(lift_expr(db, trx, inner)?))),
        Expr::Binary(op, l, r) => Ok(Expr::Binary(
            *op,
            Box::new(lift_expr(db, trx, l)?),
            Box::new(lift_expr(db, trx, r)?),
        )),
        Expr::IsNull(inner, negated) => {
            Ok(Expr::IsNull(Box::new(lift_expr(db, trx, inner)?), *negated))
        }
        Expr::Like { expr, pattern, negated } => Ok(Expr::Like {
            expr: Box::new(lift_expr(db, trx, expr)?),
            pattern: Box::new(lift_expr(db, trx, pattern)?),
            negated: *negated,
        }),
        Expr::Function(name, args) => {
            let mut lifted = Vec::with_capacity(args.len());
            for a in args {
                lifted.push(lift_expr(db, trx, a)?);
            }
            Ok(Expr::Function(name.clone(), lifted))
        }
        Expr::Aggregate(f, Some(inner)) => {
            Ok(Expr::Aggregate(*f, Some(Box::new(lift_expr(db, trx, inner)?))))
        }
        other => Ok(other.clone()),
    }
}

fn run_subquery(
    db: &mut Database,
    trx: &mut TrxState,
    sub: &SelectStmt,
) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    match execute_select(db, trx, sub)? {
        ResultSet::Rows { columns, rows } => Ok((columns, rows)),
        ResultSet::Message(_) => Err(Error::Runtime("subquery must be a select".into())),
    }
}
