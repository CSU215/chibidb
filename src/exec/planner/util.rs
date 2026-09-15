//! Pure AST helpers shared by translation, optimization and lowering.
//!
//! These operate only on the parsed syntax tree (never on catalog or physical
//! operators), so both the logical layer and the lowering layer can use them
//! without one depending on the other.

use crate::sql::ast::{BinOp, Expr, JoinKind, SelectItem, SelectStmt};

use crate::exec::aggregate::expr_has_aggregate;

/// Splits an `AND` chain into its conjuncts; a non-`AND` expression is one part.
pub(crate) fn split_conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Binary(BinOp::And, l, r) => {
            let mut out = split_conjuncts(l);
            out.extend(split_conjuncts(r));
            out
        }
        other => vec![other],
    }
}

/// Folds a list of predicates into a single `AND` chain; `None` when empty.
pub(crate) fn combine_and(mut parts: Vec<Expr>) -> Option<Expr> {
    let mut acc = parts.pop()?;
    while let Some(e) = parts.pop() {
        acc = Expr::Binary(BinOp::And, Box::new(e), Box::new(acc));
    }
    Some(acc)
}

/// The join kind and optional ON clause for every table after the first, in
/// order. A comma join is `Cross` with no ON; an explicit join consumes the
/// next entry of `select.on`, keeping both vectors aligned even when commas
/// and explicit joins are mixed.
pub(crate) fn join_clauses(select: &SelectStmt) -> Vec<(JoinKind, Option<Expr>)> {
    let mut out = Vec::new();
    let mut on_index = 0usize;
    for i in 1..select.from.len() {
        let kind = select.joins.get(i).copied().unwrap_or(JoinKind::Cross);
        let on = if kind == JoinKind::Cross {
            None
        } else {
            let on = select.on.get(on_index).cloned();
            on_index += 1;
            on
        };
        out.push((kind, on));
    }
    out
}

/// Whether any SELECT item contains an aggregate call.
pub(crate) fn items_have_aggregate(items: &[SelectItem]) -> bool {
    items.iter().any(|item| match item {
        SelectItem::Expr(e) | SelectItem::Aliased(e, _) => expr_has_aggregate(e),
        SelectItem::Star => false,
    })
}
