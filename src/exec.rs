use crate::ast::{
    BinOp, CreateTableStmt, DataType, DeleteStmt, Expr, InsertStmt, SelectItem, SelectStmt,
    Stmt, UnOp, UpdateStmt,
};
use crate::catalog::Schema;
use crate::result::ResultSet;
use crate::value::Value;
use crate::{Database, Error, Result};

pub fn execute(db: &mut Database, stmt: &Stmt) -> Result<ResultSet> {
    match stmt {
        Stmt::CreateTable(c) => execute_create_table(db, c),
        Stmt::Insert(i) => execute_insert(db, i),
        Stmt::Select(s) => execute_select(db, s),
        Stmt::Delete(d) => execute_delete(db, d),
        Stmt::Update(u) => execute_update(db, u),
    }
}

fn execute_update(db: &mut Database, u: &UpdateStmt) -> Result<ResultSet> {
    let schema = db.catalog().table(&u.table)?.schema.clone();
    let mut assigns = Vec::new();
    for (col, expr) in &u.assignments {
        let idx = schema
            .index_of(col)
            .ok_or_else(|| Error::Runtime(format!("no such column: {col}")))?;
        assigns.push((idx, col.clone(), schema.columns[idx].dtype, expr));
    }
    let scan = db.store_scan(&u.table)?;
    let mut updates = Vec::new();
    for (rid, row) in scan {
        let matched = match &u.selection {
            Some(sel) => eval_predicate(sel, &schema, &row)?,
            None => true,
        };
        if !matched {
            continue;
        }
        let mut new_row = row.clone();
        for (idx, col, dtype, expr) in &assigns {
            let v = eval(expr, Some((&schema, &row)))?;
            new_row[*idx] = coerce(v, *dtype, col)?;
        }
        updates.push((rid, new_row));
    }
    db.store_replace_all(&u.table, updates)?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn execute_delete(db: &mut Database, d: &DeleteStmt) -> Result<ResultSet> {
    let schema = db.catalog().table(&d.table)?.schema.clone();
    let scan = db.store_scan(&d.table)?;
    let mut to_delete = Vec::new();
    for (rid, row) in scan {
        let matched = match &d.selection {
            Some(sel) => eval_predicate(sel, &schema, &row)?,
            None => true,
        };
        if matched {
            to_delete.push(rid);
        }
    }
    db.store_delete_all(&d.table, &to_delete)?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn eval_predicate(expr: &Expr, schema: &Schema, row: &[Value]) -> Result<bool> {
    match eval(expr, Some((schema, row)))? {
        Value::Bool(b) => Ok(b),
        Value::Null => Ok(false),
        _ => Err(Error::Runtime(
            "where clause must evaluate to boolean".into(),
        )),
    }
}

fn execute_insert(db: &mut Database, i: &InsertStmt) -> Result<ResultSet> {
    let schema = db.catalog().table(&i.table)?.schema.clone();
    for values in &i.rows {
        if values.len() != schema.columns.len() {
            return Err(Error::Runtime(format!(
                "expected {} values, got {}",
                schema.columns.len(),
                values.len()
            )));
        }
    }
    for values in &i.rows {
        let mut row = Vec::with_capacity(values.len());
        for (expr, col) in values.iter().zip(&schema.columns) {
            let v = eval_const(expr)?;
            row.push(coerce(v, col.dtype, &col.name)?);
        }
        let encoded = crate::storage::codec::encode_row(&row);
        if encoded.len() + 8 > crate::storage::PAGE_SIZE {
            return Err(Error::Runtime(format!(
                "record too large ({} bytes does not fit in a page)",
                encoded.len()
            )));
        }
        db.store_insert(&i.table, row)?;
    }
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn coerce(v: Value, dtype: DataType, col: &str) -> Result<Value> {
    match (v, dtype) {
        (Value::Null, _) => Ok(Value::Null),
        (v @ Value::Int(_), DataType::Int) => Ok(v),
        (Value::Int(n), DataType::Float) => Ok(Value::Float(n as f64)),
        (v @ Value::Float(_), DataType::Float) => Ok(v),
        (Value::Str(s), DataType::Char(n)) => {
            if s.chars().count() <= n as usize {
                Ok(Value::Str(s))
            } else {
                Err(Error::Runtime(format!(
                    "cannot insert '{s}' into column {col}"
                )))
            }
        }
        (v @ Value::Date(_), DataType::Date) => Ok(v),
        (v @ Value::Str(_), DataType::Text) => Ok(v),
        (Value::Str(s), DataType::Date) => crate::datetime::parse_date(&s)
            .map(Value::Date)
            .map_err(|e| Error::Runtime(format!("cannot insert into column {col}: {e}"))),
        (v, _) => Err(Error::Runtime(format!(
            "cannot insert {v} into column {col}"
        ))),
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
    let heap = db.new_table_heap(&c.name)?;
    db.catalog_mut().create_table(&c.name, schema, heap)?;
    db.save_catalog()?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn execute_select(db: &mut Database, s: &SelectStmt) -> Result<ResultSet> {
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
    let schema = db.catalog().table(&from.name)?.schema.clone();
    let mut headers = Vec::new();
    let mut exprs = Vec::new();
    for item in &s.items {
        match item {
            SelectItem::Star => {
                for col in &schema.columns {
                    headers.push(col.name.clone());
                    exprs.push(Expr::Column(col.name.clone()));
                }
            }
            SelectItem::Expr(e) => {
                headers.push(e.to_string());
                exprs.push(e.clone());
            }
        }
    }
    let scan = db.store_scan(&from.name)?;
    let mut out_rows = Vec::new();
    for (_, row) in scan {
        if let Some(sel) = &s.selection {
            if !eval_predicate(sel, &schema, &row)? {
                continue;
            }
        }
        let mut out_row = Vec::with_capacity(exprs.len());
        for e in &exprs {
            out_row.push(eval(e, Some((&schema, &row)))?);
        }
        out_rows.push(out_row);
    }
    Ok(ResultSet::Rows { columns: headers, rows: out_rows })
}

pub fn eval_const(expr: &Expr) -> Result<Value> {
    eval(expr, None)
}

pub(crate) fn eval(expr: &Expr, ctx: Option<(&Schema, &[Value])>) -> Result<Value> {
    match expr {
        Expr::Int(n) => Ok(Value::Int(*n)),
        Expr::Float(x) => Ok(Value::Float(*x)),
        Expr::Str(s) => Ok(Value::Str(s.clone())),
        Expr::Null => Ok(Value::Null),
        Expr::Column(c) => {
            let Some((schema, row)) = ctx else {
                return Err(Error::Runtime(format!("no such column: {c}")));
            };
            let idx = schema
                .index_of(c)
                .ok_or_else(|| Error::Runtime(format!("no such column: {c}")))?;
            Ok(row[idx].clone())
        }
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
    }
}

fn eval_binary(op: BinOp, l: Value, r: Value) -> Result<Value> {
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
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => arith(op, l, r),
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
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    let ord: Option<std::cmp::Ordering> = match (&l, &r) {
        (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
        (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
        (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
        (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        (Value::Date(a), Value::Date(b)) => Some(a.cmp(b)),
        (Value::Date(a), Value::Str(b)) | (Value::Str(b), Value::Date(a)) => {
            match crate::datetime::parse_date(b) {
                Ok(d) => Some(a.cmp(&d)),
                Err(e) => return Err(e),
            }
        }
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
