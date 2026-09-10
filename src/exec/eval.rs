use crate::ast::{BinOp, Expr, UnOp};
use crate::catalog::Schema;
use crate::value::Value;
use crate::{Error, Result};

use super::aggregate::eval_aggregate;

pub(crate) enum EvalCtx<'a> {
    Row(&'a Schema, &'a [Value]),
    Group(&'a Schema, &'a [Vec<Value>]),
}

pub fn eval_const(expr: &Expr) -> Result<Value> {
    eval(expr, None)
}

pub(crate) fn eval_predicate(expr: &Expr, schema: &Schema, row: &[Value]) -> Result<bool> {
    match eval(expr, Some(&EvalCtx::Row(schema, row)))? {
        Value::Bool(b) => Ok(b),
        Value::Null => Ok(false),
        _ => Err(Error::Runtime(
            "where clause must evaluate to boolean".into(),
        )),
    }
}

pub(crate) fn expr_has_column(expr: &Expr) -> bool {
    match expr {
        Expr::Column(_) | Expr::QualifiedColumn(..) => true,
        Expr::Unary(_, e) => expr_has_column(e),
        Expr::Binary(_, l, r) => expr_has_column(l) || expr_has_column(r),
        Expr::IsNull(e, _) => expr_has_column(e),
        Expr::Like { expr, pattern, .. } => expr_has_column(expr) || expr_has_column(pattern),
        Expr::Function(_, args) => args.iter().any(expr_has_column),
        _ => false,
    }
}

pub(crate) fn eval(expr: &Expr, ctx: Option<&EvalCtx>) -> Result<Value> {
    match expr {
        Expr::Int(n) => Ok(Value::Int(*n)),
        Expr::Float(x) => Ok(Value::Float(*x)),
        Expr::Str(s) => Ok(Value::Str(s.clone())),
        Expr::Null => Ok(Value::Null),
        Expr::Column(c) => match ctx {
            None => Err(Error::Runtime(format!("no such column: {c}"))),
            Some(EvalCtx::Row(schema, row)) => {
                let idx = schema.resolve(None, c)?;
                Ok(row[idx].clone())
            }
            Some(EvalCtx::Group(schema, rows)) => {
                let idx = schema.resolve(None, c)?;
                Ok(rows
                    .first()
                    .map(|row| row[idx].clone())
                    .unwrap_or(Value::Null))
            }
        },
        Expr::QualifiedColumn(t, c) => match ctx {
            None => Err(Error::Runtime(format!("no such column: {t}.{c}"))),
            Some(EvalCtx::Row(schema, row)) => {
                let idx = schema.resolve(Some(t), c)?;
                Ok(row[idx].clone())
            }
            Some(EvalCtx::Group(schema, rows)) => {
                let idx = schema.resolve(Some(t), c)?;
                Ok(rows
                    .first()
                    .map(|row| row[idx].clone())
                    .unwrap_or(Value::Null))
            }
        },
        Expr::Aggregate(func, arg) => match ctx {
            Some(&EvalCtx::Group(schema, rows)) => eval_aggregate(*func, arg.as_deref(), schema, rows),
            _ => Err(Error::Runtime("aggregate not allowed here".into())),
        },
        Expr::Unary(op, e) => {
            let v = eval(e, ctx)?;
            match op {
                UnOp::Neg => match v {
                    Value::Null => Ok(Value::Null),
                    Value::Int(n) => n
                        .checked_neg()
                        .map(Value::Int)
                        .ok_or_else(|| Error::Runtime("integer overflow".into())),
                    Value::Float(x) => Ok(Value::Float(-x)),
                    _ => Err(type_mismatch()),
                },
                UnOp::Not => match v {
                    Value::Null => Ok(Value::Null),
                    Value::Bool(b) => Ok(Value::Bool(!b)),
                    _ => Err(type_mismatch()),
                },
            }
        }
        Expr::Binary(op, l, r) => {
            let lv = eval(l, ctx)?;
            let rv = eval(r, ctx)?;
            eval_binary(*op, lv, rv)
        }
        Expr::IsNull(e, negated) => {
            let v = eval(e, ctx)?;
            let is_null = matches!(v, Value::Null);
            Ok(Value::Bool(if *negated { !is_null } else { is_null }))
        }
        Expr::Like { expr, pattern, negated } => {
            let v = eval(expr, ctx)?;
            let p = eval(pattern, ctx)?;
            match (&v, &p) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(pat)) => {
                    let matched = like_match(pat, s);
                    Ok(Value::Bool(if *negated { !matched } else { matched }))
                }
                _ => Err(type_mismatch()),
            }
        }
        Expr::Function(name, args) => eval_function(name, args, ctx),
        Expr::Value(v) => Ok(v.clone()),
        Expr::InSubquery { .. } | Expr::Exists { .. } | Expr::ScalarSubquery(_) => {
            Err(Error::Runtime("subqueries are materialized before evaluation".into()))
        }
    }
}

pub(crate) fn eval_binary(op: BinOp, l: Value, r: Value) -> Result<Value> {
    match op {
        BinOp::And => match (l, r) {
            (Value::Bool(false), _) | (_, Value::Bool(false)) => Ok(Value::Bool(false)),
            (Value::Bool(true), Value::Bool(true)) => Ok(Value::Bool(true)),
            (Value::Bool(true), Value::Null) | (Value::Null, Value::Bool(true)) => {
                Ok(Value::Null)
            }
            (Value::Null, Value::Null) => Ok(Value::Null),
            _ => Err(type_mismatch()),
        },
        BinOp::Or => match (l, r) {
            (Value::Bool(true), _) | (_, Value::Bool(true)) => Ok(Value::Bool(true)),
            (Value::Bool(false), Value::Bool(false)) => Ok(Value::Bool(false)),
            (Value::Bool(false), Value::Null) | (Value::Null, Value::Bool(false)) => {
                Ok(Value::Null)
            }
            (Value::Null, Value::Null) => Ok(Value::Null),
            _ => Err(type_mismatch()),
        },
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => arith(op, l, r),
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            compare(op, l, r)
        }
    }
}

fn arith(op: BinOp, l: Value, r: Value) -> Result<Value> {
    match (l, r) {
        (Value::Null, Value::Null) => Ok(Value::Null),
        (Value::Null, Value::Int(_) | Value::Float(_))
        | (Value::Int(_) | Value::Float(_), Value::Null) => Ok(Value::Null),
        (Value::Int(a), Value::Int(b)) => int_arith(op, a, b),
        (Value::Float(a), Value::Float(b)) => float_arith(op, a, b),
        (Value::Int(a), Value::Float(b)) => float_arith(op, a as f64, b),
        (Value::Float(a), Value::Int(b)) => float_arith(op, a, b as f64),
        _ => Err(type_mismatch()),
    }
}

fn int_arith(op: BinOp, a: i64, b: i64) -> Result<Value> {
    let checked = match op {
        BinOp::Add => a.checked_add(b),
        BinOp::Sub => a.checked_sub(b),
        BinOp::Mul => a.checked_mul(b),
        BinOp::Div => {
            if b == 0 {
                return Err(div_by_zero());
            }
            a.checked_div(b)
        }
        BinOp::Mod => {
            if b == 0 {
                return Err(div_by_zero());
            }
            a.checked_rem(b)
        }
        _ => unreachable!(),
    };
    checked
        .map(Value::Int)
        .ok_or_else(|| Error::Runtime("integer overflow".into()))
}

fn float_arith(op: BinOp, a: f64, b: f64) -> Result<Value> {
    if (op == BinOp::Div || op == BinOp::Mod) && b == 0.0 {
        return Err(div_by_zero());
    }
    let v = match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => a / b,
        BinOp::Mod => a % b,
        _ => unreachable!(),
    };
    Ok(Value::Float(v))
}

/// SQL LIKE: `%` matches any run of characters (including none), `_`
/// matches exactly one character. Matching is case-sensitive and there is
/// no escape character. Runs in O(text * pattern) time and space.
fn like_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    let (n, m) = (txt.len(), pat.len());
    let mut prev = vec![false; m + 1];
    prev[0] = true;
    for (j, p) in pat.iter().enumerate() {
        if *p == '%' {
            prev[j + 1] = true;
        } else {
            break;
        }
    }
    for i in 1..=n {
        let mut cur = vec![false; m + 1];
        for j in 1..=m {
            cur[j] = match pat[j - 1] {
                '%' => cur[j - 1] || prev[j],
                '_' => prev[j - 1],
                c => prev[j - 1] && txt[i - 1] == c,
            };
        }
        prev = cur;
    }
    prev[m]
}

pub(crate) fn cmp_values(l: &Value, r: &Value) -> Result<Option<std::cmp::Ordering>> {
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(None);
    }
    let ord = match (l, r) {
        (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
        (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
        (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
        (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        (Value::Date(a), Value::Date(b)) => Some(a.cmp(b)),
        (Value::Date(a), Value::Str(b)) => Some(a.cmp(&crate::datetime::parse_date(b)?)),
        (Value::Str(a), Value::Date(b)) => Some(crate::datetime::parse_date(a)?.cmp(b)),
        _ => return Err(type_mismatch()),
    };
    Ok(ord)
}

fn compare(op: BinOp, l: Value, r: Value) -> Result<Value> {
    use std::cmp::Ordering::{Equal, Greater, Less};
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    let res = match cmp_values(&l, &r)? {
        None => matches!(op, BinOp::NotEq),
        Some(Less) => matches!(op, BinOp::Lt | BinOp::Le | BinOp::NotEq),
        Some(Equal) => matches!(op, BinOp::Le | BinOp::Ge | BinOp::Eq),
        Some(Greater) => matches!(op, BinOp::Gt | BinOp::Ge | BinOp::NotEq),
    };
    Ok(Value::Bool(res))
}

/// Scalar string functions. NULL arguments propagate to a NULL result;
/// UPPER/LOWER/LENGTH/SUBSTRING accept strings only, CONCAT coerces any
/// scalar to its text form.
fn eval_function(name: &str, args: &[Expr], ctx: Option<&EvalCtx>) -> Result<Value> {
    let mut vals = Vec::with_capacity(args.len());
    for a in args {
        vals.push(eval(a, ctx)?);
    }
    match name {
        "concat" => {
            let mut out = String::new();
            for v in &vals {
                match v {
                    Value::Null => return Ok(Value::Null),
                    Value::Str(s) => out.push_str(s),
                    Value::Int(_) | Value::Float(_) | Value::Date(_) | Value::Bool(_) => {
                        out.push_str(&v.to_string())
                    }
                }
            }
            Ok(Value::Str(out))
        }
        "upper" | "lower" => {
            let [v] = vals.as_slice() else {
                return Err(arity_error(name, vals.len(), "1"));
            };
            match str_arg(v)? {
                None => Ok(Value::Null),
                Some(s) => Ok(Value::Str(if name == "upper" {
                    s.to_uppercase()
                } else {
                    s.to_lowercase()
                })),
            }
        }
        "length" => {
            let [v] = vals.as_slice() else {
                return Err(arity_error(name, vals.len(), "1"));
            };
            match str_arg(v)? {
                None => Ok(Value::Null),
                Some(s) => Ok(Value::Int(s.chars().count() as i64)),
            }
        }
        "substring" | "substr" => {
            if vals.len() != 2 && vals.len() != 3 {
                return Err(arity_error(name, vals.len(), "2 or 3"));
            }
            let Some(s) = str_arg(&vals[0])? else {
                return Ok(Value::Null);
            };
            let Some(start) = int_arg(&vals[1])? else {
                return Ok(Value::Null);
            };
            if start < 1 {
                return Err(Error::Runtime("substring start must be positive".into()));
            }
            let len = match vals.get(2) {
                Some(v) => match int_arg(v)? {
                    None => return Ok(Value::Null),
                    Some(n) if n >= 0 => Some(n as usize),
                    Some(_) => {
                        return Err(Error::Runtime(
                            "substring length must be non-negative".into(),
                        ))
                    }
                },
                None => None,
            };
            let skipped: String = s.chars().skip((start - 1) as usize).collect();
            let taken: String = match len {
                Some(n) => skipped.chars().take(n).collect(),
                None => skipped,
            };
            Ok(Value::Str(taken))
        }
        other => Err(Error::Runtime(format!("unknown function: {other}"))),
    }
}

fn str_arg(v: &Value) -> Result<Option<&str>> {
    match v {
        Value::Null => Ok(None),
        Value::Str(s) => Ok(Some(s)),
        _ => Err(type_mismatch()),
    }
}

fn int_arg(v: &Value) -> Result<Option<i64>> {
    match v {
        Value::Null => Ok(None),
        Value::Int(n) => Ok(Some(*n)),
        _ => Err(type_mismatch()),
    }
}

fn arity_error(name: &str, got: usize, want: &str) -> Error {
    Error::Runtime(format!("{name} expects {want} argument(s), got {got}"))
}

pub(crate) fn type_mismatch() -> Error {
    Error::Runtime("type mismatch in expression".into())
}

fn div_by_zero() -> Error {
    Error::Runtime("division by zero".into())
}
