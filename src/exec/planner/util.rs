//! Pure AST helpers shared by translation, optimization and lowering.
//!
//! These operate only on the parsed syntax tree (never on catalog or physical
//! operators), so both the logical layer and the lowering layer can use them
//! without one depending on the other.

use crate::catalog::Schema;
use crate::sql::ast::{BinOp, Expr, JoinKind, SelectItem, SelectStmt};
use crate::value::DataType;

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

/// Dtype of a simple column reference, used to reject join keys whose numerics
/// would need coercion (the index key encoding is type-sensitive).
fn column_dtype(schema: &Schema, expr: &Expr) -> Option<DataType> {
    match expr {
        Expr::Column(name) => schema.columns.iter().find(|c| &c.name == name).map(|c| c.dtype),
        Expr::QualifiedColumn(owner, name) => schema
            .columns
            .iter()
            .find(|c| c.owner.as_deref() == Some(owner) && &c.name == name)
            .map(|c| c.dtype),
        _ => None,
    }
}

fn types_compatible(a: DataType, b: DataType) -> bool {
    matches!(
        (a, b),
        (DataType::Int, DataType::Int)
            | (DataType::Float, DataType::Float)
            | (DataType::Date, DataType::Date)
            | (DataType::Text, DataType::Text)
            | (DataType::Char(_), DataType::Char(_))
    )
}

/// If `expr` is an equality between a plain column of `left` and one of
/// `right` with compatible types, returns the `(left_expr, right_expr)` pair.
/// Shared by the logical join-forming rewrite and the physical hash-join key
/// extraction, so both agree on what counts as an equi-join predicate.
pub(crate) fn equi_join_keys<'a>(
    expr: &'a Expr,
    left: &Schema,
    right: &Schema,
) -> Option<(&'a Expr, &'a Expr)> {
    let Expr::Binary(BinOp::Eq, a, b) = expr else {
        return None;
    };
    if let (Some(ld), Some(rd)) = (column_dtype(left, a), column_dtype(right, b))
        && types_compatible(ld, rd)
    {
        return Some((a, b));
    }
    if let (Some(ld), Some(rd)) = (column_dtype(left, b), column_dtype(right, a))
        && types_compatible(ld, rd)
    {
        return Some((b, a));
    }
    None
}
