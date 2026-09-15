//! Rule-based expression rewrites applied once per statement before planning.
//!
//! The only pass here is constant folding: any compound subexpression that needs
//! no row context (`WHERE k = 1 + 2`, `HAVING sum(x) + 0`, ...) is evaluated to a
//! literal. It is deliberately conservative: an evaluation failure (division by
//! zero, type error) leaves the expression untouched so the error still surfaces
//! at the same point as before, and subqueries are folded recursively rather
//! than executed.
//!
//! SELECT items are intentionally left alone: folding them would change the
//! result column headers (`1+2` -> `3`), which are part of the observable output.

use crate::sql::ast::{BinOp, DeleteStmt, Expr, InsertStmt, SelectStmt, Stmt, UpdateStmt};
use crate::value::Value;

use super::eval::{eval_const, expr_has_column, expr_has_subquery};

/// Returns a copy of `stmt` with its constant subexpressions folded.
pub(crate) fn fold_stmt(stmt: &Stmt) -> Stmt {
    match stmt {
        Stmt::Select(s) => Stmt::Select(Box::new(fold_select(s))),
        Stmt::Insert(i) => Stmt::Insert(InsertStmt {
            table: i.table.clone(),
            columns: i.columns.clone(),
            rows: i.rows.iter().map(|row| row.iter().map(fold_expr).collect()).collect(),
        }),
        Stmt::Update(u) => Stmt::Update(UpdateStmt {
            table: u.table.clone(),
            assignments: u
                .assignments
                .iter()
                .map(|(c, e)| (c.clone(), fold_expr(e)))
                .collect(),
            selection: u.selection.as_ref().map(fold_expr),
        }),
        Stmt::Delete(d) => Stmt::Delete(DeleteStmt {
            table: d.table.clone(),
            selection: d.selection.as_ref().map(fold_expr),
        }),
        other => other.clone(),
    }
}

fn fold_select(s: &SelectStmt) -> SelectStmt {
    SelectStmt {
        distinct: s.distinct,
        items: s.items.clone(),
        from: s.from.clone(),
        joins: s.joins.clone(),
        on: s.on.iter().map(fold_expr).collect(),
        selection: s.selection.as_ref().map(fold_expr),
        group_by: s.group_by.iter().map(fold_expr).collect(),
        having: s.having.as_ref().map(fold_expr),
        order_by: s.order_by.iter().map(|(e, desc)| (fold_expr(e), *desc)).collect(),
        limit: s.limit.clone(),
        set_ops: s
            .set_ops
            .iter()
            .map(|(all, sub)| (*all, Box::new(fold_select(sub))))
            .collect(),
    }
}

/// Folds `e` bottom-up, recursing into subqueries so their bodies are folded
/// too but never executing them.
fn fold_expr(e: &Expr) -> Expr {
    let folded = match e {
        Expr::Unary(op, a) => Expr::Unary(*op, Box::new(fold_expr(a))),
        Expr::Binary(op, l, r) => {
            Expr::Binary(*op, Box::new(fold_expr(l)), Box::new(fold_expr(r)))
        }
        Expr::IsNull(a, negated) => Expr::IsNull(Box::new(fold_expr(a)), *negated),
        Expr::Like { expr, pattern, negated, escape } => Expr::Like {
            expr: Box::new(fold_expr(expr)),
            pattern: Box::new(fold_expr(pattern)),
            negated: *negated,
            escape: *escape,
        },
        Expr::Function(name, args) => {
            Expr::Function(name.clone(), args.iter().map(fold_expr).collect())
        }
        Expr::Aggregate(func, arg, distinct) => {
            Expr::Aggregate(*func, arg.as_ref().map(|a| Box::new(fold_expr(a))), *distinct)
        }
        Expr::InSubquery { expr, sub, negated } => Expr::InSubquery {
            expr: Box::new(fold_expr(expr)),
            sub: Box::new(fold_select(sub)),
            negated: *negated,
        },
        Expr::Exists { sub } => Expr::Exists { sub: Box::new(fold_select(sub)) },
        Expr::ScalarSubquery(sub) => Expr::ScalarSubquery(Box::new(fold_select(sub))),
        // Literals and column references are already minimal.
        other => other.clone(),
    };
    let compound = matches!(
        folded,
        Expr::Unary(..)
            | Expr::Binary(..)
            | Expr::IsNull(..)
            | Expr::Like { .. }
            | Expr::Function(..)
    );
    if compound
        && !expr_has_column(&folded)
        && !expr_has_subquery(&folded)
        && let Ok(value) = eval_const(&folded)
    {
        return Expr::Value(value);
    }
    simplify(folded)
}

/// Applies boolean identities over an already-folded node. Constant folding
/// turns `1=1` into a `Bool` literal; this drops it again where the surrounding
/// `AND`/`OR` makes it redundant, so a conjunct list is not silently neutered.
fn simplify(e: Expr) -> Expr {
    if let Expr::Binary(op @ (BinOp::And | BinOp::Or), l, r) = &e {
        let left = bool_lit(l);
        let right = bool_lit(r);
        match op {
            // `x AND false` is false in SQL even when `x` is NULL.
            BinOp::And if left == Some(false) || right == Some(false) => {
                return Expr::Value(Value::Bool(false));
            }
            BinOp::And if left == Some(true) => return (**r).clone(),
            BinOp::And if right == Some(true) => return (**l).clone(),
            // `x OR true` is true even when `x` is NULL.
            BinOp::Or if left == Some(true) || right == Some(true) => {
                return Expr::Value(Value::Bool(true));
            }
            BinOp::Or if left == Some(false) => return (**r).clone(),
            BinOp::Or if right == Some(false) => return (**l).clone(),
            _ => {}
        }
    }
    e
}

fn bool_lit(e: &Expr) -> Option<bool> {
    match e {
        Expr::Value(Value::Bool(b)) => Some(*b),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::parser;
    use crate::value::Value;

    fn select(sql: &str) -> SelectStmt {
        match parser::parse(sql).unwrap().remove(0) {
            Stmt::Select(s) => *s,
            other => panic!("expected select, got {other:?}"),
        }
    }

    #[test]
    fn folds_an_arithmetic_predicate() {
        let s = select("select id from t where id = 1 + 2;");
        let folded = fold_select(&s);
        assert_eq!(folded.selection, Some(Expr::Binary(
            crate::sql::ast::BinOp::Eq,
            Box::new(Expr::Column("id".into())),
            Box::new(Expr::Value(Value::Int(3))),
        )));
    }

    #[test]
    fn keeps_a_column_dependent_expression() {
        let s = select("select id from t where id = id + 1;");
        let folded = fold_select(&s);
        // the column side stops folding; the tree is unchanged in shape
        assert_eq!(folded.selection, s.selection);
    }

    #[test]
    fn leaves_division_by_zero_for_runtime() {
        let s = select("select id from t where id = 1 / 0;");
        let folded = fold_select(&s);
        assert!(matches!(folded.selection, Some(Expr::Binary(..))));
    }

    #[test]
    fn folds_inside_a_subquery_body() {
        let s = select("select id from t where id in (select id from u where x = 2 + 3);");
        let folded = fold_select(&s);
        let Some(Expr::InSubquery { sub, .. }) = &folded.selection else {
            panic!("expected an IN subquery, got {:?}", folded.selection);
        };
        assert!(matches!(
            sub.selection,
            Some(Expr::Binary(crate::sql::ast::BinOp::Eq, _, ref r)) if **r == Expr::Value(Value::Int(5))
        ));
    }

    #[test]
    fn drops_a_redundant_true_conjunct() {
        let s = select("select id from t where id > 0 and 1 = 1;");
        let folded = fold_select(&s);
        assert_eq!(
            folded.selection,
            Some(Expr::Binary(
                BinOp::Gt,
                Box::new(Expr::Column("id".into())),
                Box::new(Expr::Int(0)),
            ))
        );
    }

    #[test]
    fn folds_a_false_predicate_to_false() {
        let s = select("select id from t where 1 = 0;");
        let folded = fold_select(&s);
        assert_eq!(folded.selection, Some(Expr::Value(Value::Bool(false))));
    }
}
