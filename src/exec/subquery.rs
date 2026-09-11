use crate::ast::{BinOp, Expr, UnOp};
use crate::catalog::Schema;
use crate::trx::TrxState;
use crate::value::Value;
use crate::{Database, Error, Result};

use super::eval::{eval, expr_has_subquery, EvalCtx};

/// Replaces every subquery in `e` with the literal value it evaluates to,
/// executing it against `outer` so correlated references resolve. The
/// result contains no subquery nodes and can be evaluated purely.
pub(crate) fn bind_expr(
    db: &Database,
    trx: &mut TrxState,
    e: &Expr,
    outer: Option<&EvalCtx>,
) -> Result<Expr> {
    Ok(match e {
        Expr::InSubquery { expr, sub, negated } => {
            let inner = bind_expr(db, trx, expr, outer)?;
            let (cols, rows) = run_subquery(db, trx, sub, outer)?;
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
            if *negated {
                Expr::Unary(UnOp::Not, Box::new(folded))
            } else {
                folded
            }
        }
        Expr::Exists { sub } => {
            let (_, rows) = run_subquery(db, trx, sub, outer)?;
            Expr::Value(Value::Bool(!rows.is_empty()))
        }
        Expr::ScalarSubquery(sub) => {
            let (cols, rows) = run_subquery(db, trx, sub, outer)?;
            if cols.len() != 1 {
                return Err(Error::Runtime(
                    "scalar subquery must return a single column".into(),
                ));
            }
            match rows.len() {
                0 => Expr::Value(Value::Null),
                1 => Expr::Value(
                    rows.into_iter()
                        .next()
                        .expect("one row")
                        .into_iter()
                        .next()
                        .expect("one column"),
                ),
                _ => {
                    return Err(Error::Runtime(
                        "scalar subquery returned more than one row".into(),
                    ))
                }
            }
        }
        Expr::Unary(op, inner) => {
            Expr::Unary(*op, Box::new(bind_expr(db, trx, inner, outer)?))
        }
        Expr::Binary(op, l, r) => Expr::Binary(
            *op,
            Box::new(bind_expr(db, trx, l, outer)?),
            Box::new(bind_expr(db, trx, r, outer)?),
        ),
        Expr::IsNull(inner, negated) => {
            Expr::IsNull(Box::new(bind_expr(db, trx, inner, outer)?), *negated)
        }
        Expr::Like { expr, pattern, negated, escape } => Expr::Like {
            expr: Box::new(bind_expr(db, trx, expr, outer)?),
            pattern: Box::new(bind_expr(db, trx, pattern, outer)?),
            negated: *negated,
            escape: *escape,
        },
        Expr::Function(name, args) => {
            let mut bound = Vec::with_capacity(args.len());
            for a in args {
                bound.push(bind_expr(db, trx, a, outer)?);
            }
            Expr::Function(name.clone(), bound)
        }
        Expr::Aggregate(f, Some(inner), distinct) => Expr::Aggregate(
            *f,
            Some(Box::new(bind_expr(db, trx, inner, outer)?)),
            *distinct,
        ),
        other => other.clone(),
    })
}

/// Evaluates `e`, first materializing any subqueries against `ctx`.
pub(crate) fn eval_bound(
    db: &Database,
    trx: &mut TrxState,
    e: &Expr,
    ctx: Option<&EvalCtx>,
) -> Result<Value> {
    if !expr_has_subquery(e) {
        return eval(e, ctx);
    }
    let bound = bind_expr(db, trx, e, ctx)?;
    eval(&bound, ctx)
}

pub(crate) fn eval_predicate_bound(
    db: &Database,
    trx: &mut TrxState,
    e: &Expr,
    schema: &Schema,
    row: &[Value],
    outer: Option<&EvalCtx>,
) -> Result<bool> {
    let mut ctx = EvalCtx::row(schema, row);
    ctx.parent = outer;
    match eval_bound(db, trx, e, Some(&ctx))? {
        Value::Bool(b) => Ok(b),
        Value::Null => Ok(false),
        _ => Err(Error::Runtime(
            "where clause must evaluate to boolean".into(),
        )),
    }
}

fn run_subquery(
    db: &Database,
    trx: &mut TrxState,
    sub: &crate::ast::SelectStmt,
    outer: Option<&EvalCtx>,
) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    let Some(mut plan) = crate::exec::operator::build_select(db, sub)? else {
        return Err(Error::Runtime(
            "subquery shape is not supported by the operators".into(),
        ));
    };
    let columns: Vec<String> = plan.schema().columns.iter().map(|c| c.name.clone()).collect();
    let mut rows = Vec::new();
    {
        let mut ctx = crate::exec::operator::ExecContext { db, trx, outer };
        plan.open(&mut ctx)?;
        while let Some(row) = plan.next(&mut ctx)? {
            rows.push(row);
        }
        plan.close()?;
    }
    Ok((columns, rows))
}
