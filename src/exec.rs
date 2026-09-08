use crate::ast::{BinOp, CreateTableStmt, Expr, SelectItem, SelectStmt, Stmt, UnOp};
use crate::catalog::Schema;
use crate::result::ResultSet;
use crate::value::Value;
use crate::{Database, Error, Result};

pub fn execute(db: &mut Database, stmt: &Stmt) -> Result<ResultSet> {
    match stmt {
        Stmt::CreateTable(c) => execute_create_table(db, c),
        Stmt::Select(s) => execute_select(db, s),
        _ => Err(Error::Runtime("not implemented yet".into())),
    }
}

fn execute_create_table(db: &mut Database, c: &CreateTableStmt) -> Result<ResultSet> {
    let schema = Schema {
        columns: c
            .columns
            .iter()
            .map(|cd| crate::catalog::ColumnDesc {
                name: cd.name.clone(),
                dtype: cd.dtype,
            })
            .collect(),
    };
    db.catalog_mut().create_table(&c.name, schema)?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn execute_select(db: &Database, s: &SelectStmt) -> Result<ResultSet> {
    let Some(from) = &s.from else {
        let mut columns = Vec::new();
        let mut row = Vec::new();
        for item in &s.items {
            match item {
                SelectItem::Expr(e) => {
                    columns.push(e.to_string());
                    row.push(eval_const(e)?);
                }
                SelectItem::Star => {
                    return Err(Error::Runtime("select * requires from".into()))
                }
            }
        }
        return Ok(ResultSet::Rows { columns, rows: vec![row] });
    };
    db.catalog().table(&from.name)?;
    Err(Error::Runtime("not implemented yet".into()))
}

pub fn eval_const(expr: &Expr) -> Result<Value> {
    match expr {
        Expr::Int(n) => Ok(Value::Int(*n)),
        Expr::Float(x) => Ok(Value::Float(*x)),
        Expr::Str(s) => Ok(Value::Str(s.clone())),
        Expr::Column(c) => Err(Error::Runtime(format!("no such column: {c}"))),
        Expr::Unary(op, e) => {
            let v = eval_const(e)?;
            match op {
                UnOp::Neg => match v {
                    Value::Int(n) => n
                        .checked_neg()
                        .map(Value::Int)
                        .ok_or_else(|| Error::Runtime("integer overflow".into())),
                    Value::Float(x) => Ok(Value::Float(-x)),
                    _ => Err(type_mismatch()),
                },
                UnOp::Not => match v {
                    Value::Bool(b) => Ok(Value::Bool(!b)),
                    _ => Err(type_mismatch()),
                },
            }
        }
        Expr::Binary(op, l, r) => {
            let lv = eval_const(l)?;
            let rv = eval_const(r)?;
            eval_binary(*op, lv, rv)
        }
    }
}

fn eval_binary(op: BinOp, l: Value, r: Value) -> Result<Value> {
    match op {
        BinOp::And => match (l, r) {
            (Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a && b)),
            _ => Err(type_mismatch()),
        },
        BinOp::Or => match (l, r) {
            (Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a || b)),
            _ => Err(type_mismatch()),
        },
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => arith(op, l, r),
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            compare(op, l, r)
        }
    }
}

fn arith(op: BinOp, l: Value, r: Value) -> Result<Value> {
    match (l, r) {
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
        _ => unreachable!(),
    };
    checked
        .map(Value::Int)
        .ok_or_else(|| Error::Runtime("integer overflow".into()))
}

fn float_arith(op: BinOp, a: f64, b: f64) -> Result<Value> {
    if op == BinOp::Div && b == 0.0 {
        return Err(div_by_zero());
    }
    let v = match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => a / b,
        _ => unreachable!(),
    };
    Ok(Value::Float(v))
}

fn compare(op: BinOp, l: Value, r: Value) -> Result<Value> {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let ord: Option<std::cmp::Ordering> = match (&l, &r) {
        (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
        (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
        (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
        (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        _ => return Err(type_mismatch()),
    };
    let res = match ord {
        None => matches!(op, BinOp::NotEq),
        Some(Less) => matches!(op, BinOp::Lt | BinOp::Le | BinOp::NotEq),
        Some(Equal) => matches!(op, BinOp::Le | BinOp::Ge | BinOp::Eq),
        Some(Greater) => matches!(op, BinOp::Gt | BinOp::Ge | BinOp::NotEq),
    };
    Ok(Value::Bool(res))
}

fn type_mismatch() -> Error {
    Error::Runtime("type mismatch in expression".into())
}

fn div_by_zero() -> Error {
    Error::Runtime("division by zero".into())
}
