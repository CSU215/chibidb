use crate::ast::{
    BinOp, CreateIndexStmt, CreateTableStmt, DataType, DeleteStmt, DropIndexStmt, ExplainStmt,
    Expr, InsertStmt, SelectItem, SelectStmt, Stmt, UnOp, UpdateStmt,
};
use crate::catalog::Schema;
use crate::index::{encode_key, BTree, Bound};
use crate::result::ResultSet;
use crate::storage::Rid;
use crate::value::Value;
use crate::{Database, Error, Result};

pub fn execute(db: &mut Database, stmt: &Stmt) -> Result<ResultSet> {
    match stmt {
        Stmt::CreateTable(c) => execute_create_table(db, c),
        Stmt::CreateIndex(c) => execute_create_index(db, c),
        Stmt::DropIndex(d) => execute_drop_index(db, d),
        Stmt::Insert(i) => execute_insert(db, i),
        Stmt::Select(s) => execute_select(db, s),
        Stmt::Delete(d) => execute_delete(db, d),
        Stmt::Update(u) => execute_update(db, u),
        Stmt::Explain(e) => execute_explain(db, e),
    }
}

fn execute_explain(db: &mut Database, e: &ExplainStmt) -> Result<ResultSet> {
    match &*e.stmt {
        Stmt::Select(s) => Ok(ResultSet::Message(plan_select(db, s)?)),
        _ => Err(Error::Runtime("explain supports select only".into())),
    }
}

fn plan_select(db: &mut Database, s: &SelectStmt) -> Result<String> {
    let Some(from) = &s.from else {
        return Ok("ConstantSelect -> Project".into());
    };
    match find_sargable(db, &from.name, s.selection.as_ref())? {
        Some(sarg) => Ok(format!(
            "IndexScan(index={}, table={}, where {} {}) -> Filter -> Project",
            sarg.index, from.name, sarg.column, sarg.op
        )),
        None => Ok(format!("FullScan(table={}) -> Filter -> Project", from.name)),
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

fn expr_has_column(expr: &Expr) -> bool {
    match expr {
        Expr::Column(_) => true,
        Expr::Unary(_, e) => expr_has_column(e),
        Expr::Binary(_, l, r) => expr_has_column(l) || expr_has_column(r),
        Expr::IsNull(e, _) => expr_has_column(e),
        _ => false,
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

fn execute_create_index(db: &mut Database, c: &CreateIndexStmt) -> Result<ResultSet> {
    let schema = db.catalog().table(&c.table)?.schema.clone();
    let col_idx = schema
        .index_of(&c.column)
        .ok_or_else(|| Error::Runtime(format!("no such column: {}", c.column)))?;
    let store = db.new_index_heap(&c.name)?;
    let scan = db.store_scan(&c.table)?;
    let btree = BTree::at(store.file);
    for (rid, row) in scan {
        let key = encode_key(&row[col_idx])?;
        btree.insert(&mut db.pool, &key, rid)?;
    }
    db.catalog_mut().create_index(
        &c.name,
        c.table.clone(),
        c.column.clone(),
        store,
    )?;
    db.save_catalog()?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn execute_drop_index(db: &mut Database, d: &DropIndexStmt) -> Result<ResultSet> {
    db.catalog_mut().drop_index(&d.name)?;
    db.save_catalog()?;
    Ok(ResultSet::Message("SUCCESS".into()))
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
            let v = eval(expr, Some(&EvalCtx::Row(&schema, &row)))?;
            new_row[*idx] = coerce(v, *dtype, col)?;
        }
        updates.push((rid, row, new_row));
    }
    db.store_replace_all(&u.table, &updates)?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn execute_delete(db: &mut Database, d: &DeleteStmt) -> Result<ResultSet> {
    let schema = db.catalog().table(&d.table)?.schema.clone();
    let scan = db.store_scan(&d.table)?;
    let mut victims = Vec::new();
    for (rid, row) in scan {
        let matched = match &d.selection {
            Some(sel) => eval_predicate(sel, &schema, &row)?,
            None => true,
        };
        if matched {
            victims.push((rid, row));
        }
    }
    db.store_delete_all(&d.table, &victims)?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn eval_predicate(expr: &Expr, schema: &Schema, row: &[Value]) -> Result<bool> {
    match eval(expr, Some(&EvalCtx::Row(schema, row)))? {
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
    let sarg = find_sargable(db, &from.name, s.selection.as_ref())?;
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
    let scan = match sarg {
        Some(sarg) => {
            let lit_val = eval_const(&sarg.lit)?;
            let coerced = coerce(lit_val, sarg.dtype, &sarg.column)?;
            let key = encode_key(&coerced)?;
            let ix_file = db
                .catalog()
                .indexes_for(&from.name)
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
            db.store_get_rows(&from.name, &rids)?
        }
        None => db.store_scan(&from.name)?,
    };
    let mut filtered: Vec<Vec<Value>> = Vec::new();
    for (_, row) in scan {
        if let Some(sel) = &s.selection {
            if !eval_predicate(sel, &schema, &row)? {
                continue;
            }
        }
        filtered.push(row);
    }
    let has_aggregate = s
        .items
        .iter()
        .any(|it| matches!(it, SelectItem::Expr(e) if expr_has_aggregate(e)))
        || s.having.as_ref().is_some_and(expr_has_aggregate);

    if !s.group_by.is_empty() || has_aggregate {
        return execute_grouped_select(&schema, s, filtered, headers, exprs);
    }
    let mut out_rows = Vec::new();
    for row in filtered {
        let mut out_row = Vec::with_capacity(exprs.len());
        for e in &exprs {
            out_row.push(eval(e, Some(&EvalCtx::Row(&schema, &row)))?);
        }
        out_rows.push(out_row);
    }
    Ok(ResultSet::Rows { columns: headers, rows: out_rows })
}

fn execute_grouped_select(
    schema: &Schema,
    s: &SelectStmt,
    filtered: Vec<Vec<Value>>,
    headers: Vec<String>,
    exprs: Vec<Expr>,
) -> Result<ResultSet> {
    for g in &s.group_by {
        if expr_has_aggregate(g) {
            return Err(Error::Runtime("aggregate functions are not allowed in group by".into()));
        }
    }
    if s.group_by.is_empty() {
        for it in &s.items {
            if let SelectItem::Expr(e) = it {
                if expr_has_column(e) {
                    return Err(Error::Runtime(
                        "column must appear in group by or aggregate".into(),
                    ));
                }
            }
        }
    }
    let mut groups: Vec<(Vec<Value>, Vec<Vec<Value>>)> = Vec::new();
    if s.group_by.is_empty() {
        groups.push((vec![], filtered));
    } else {
        for row in filtered {
            let mut key = Vec::with_capacity(s.group_by.len());
            for g in &s.group_by {
                key.push(eval(g, Some(&EvalCtx::Row(schema, &row)))?);
            }
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, rows)) => rows.push(row),
                None => groups.push((key, vec![row])),
            }
        }
    }
    let mut out_rows = Vec::new();
    for (_, group_rows) in groups {
        if let Some(having) = &s.having {
            match eval(having, Some(&EvalCtx::Group(schema, &group_rows)))? {
                Value::Bool(true) => {}
                Value::Bool(false) | Value::Null => continue,
                _ => {
                    return Err(Error::Runtime(
                        "having clause must evaluate to boolean".into(),
                    ))
                }
            }
        }
        let mut out_row = Vec::with_capacity(exprs.len());
        for e in &exprs {
            out_row.push(eval(e, Some(&EvalCtx::Group(schema, &group_rows)))?);
        }
        out_rows.push(out_row);
    }
    Ok(ResultSet::Rows { columns: headers, rows: out_rows })
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

pub fn eval_const(expr: &Expr) -> Result<Value> {
    eval(expr, None)
}

pub(crate) fn expr_has_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Aggregate(..) => true,
        Expr::Unary(_, e) => expr_has_aggregate(e),
        Expr::Binary(_, l, r) => expr_has_aggregate(l) || expr_has_aggregate(r),
        Expr::IsNull(e, _) => expr_has_aggregate(e),
        _ => false,
    }
}

fn eval_aggregate(
    func: crate::ast::AggFunc,
    arg: Option<&Expr>,
    schema: &Schema,
    rows: &[Vec<Value>],
) -> Result<Value> {
    use crate::ast::AggFunc;
    let vals: Vec<Value> = match arg {
        None => vec![],
        Some(e) => {
            let mut vals = Vec::with_capacity(rows.len());
            for row in rows {
                match eval(e, Some(&EvalCtx::Row(schema, row)))? {
                    Value::Null => {}
                    v => vals.push(v),
                }
            }
            vals
        }
    };
    match func {
        AggFunc::Count => Ok(Value::Int(match arg {
            None => rows.len() as i64,
            Some(_) => vals.len() as i64,
        })),
        AggFunc::Sum => {
            let mut acc: Option<Value> = None;
            for v in vals {
                acc = Some(match acc {
                    None => v,
                    Some(a) => eval_binary(BinOp::Add, a, v)?,
                });
            }
            Ok(acc.unwrap_or(Value::Null))
        }
        AggFunc::Avg => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            let mut total = 0.0f64;
            for v in &vals {
                match v {
                    Value::Int(n) => total += *n as f64,
                    Value::Float(x) => total += *x,
                    _ => return Err(type_mismatch()),
                }
            }
            Ok(Value::Float(total / vals.len() as f64))
        }
        AggFunc::Min | AggFunc::Max => {
            let mut best: Option<&Value> = None;
            for v in &vals {
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        let ord = cmp_values(b, v)?.ok_or_else(type_mismatch)?;
                        let take = match func {
                            AggFunc::Min => ord == std::cmp::Ordering::Greater,
                            _ => ord == std::cmp::Ordering::Less,
                        };
                        if take {
                            v
                        } else {
                            b
                        }
                    }
                });
            }
            Ok(best.cloned().unwrap_or(Value::Null))
        }
    }
}

pub(crate) enum EvalCtx<'a> {
    Row(&'a Schema, &'a [Value]),
    Group(&'a Schema, &'a [Vec<Value>]),
}

pub(crate) fn eval(expr: &Expr, ctx: Option<&EvalCtx>) -> Result<Value> {
    match expr {
        Expr::Int(n) => Ok(Value::Int(*n)),
        Expr::Float(x) => Ok(Value::Float(*x)),
        Expr::Str(s) => Ok(Value::Str(s.clone())),
        Expr::Null => Ok(Value::Null),
        Expr::Column(c) => match ctx {
            None => Err(Error::Runtime(format!("no such column: {c}"))),
            Some(&EvalCtx::Row(schema, row)) => {
                let idx = schema
                    .index_of(c)
                    .ok_or_else(|| Error::Runtime(format!("no such column: {c}")))?;
                Ok(row[idx].clone())
            }
            Some(&EvalCtx::Group(schema, rows)) => {
                let idx = schema
                    .index_of(c)
                    .ok_or_else(|| Error::Runtime(format!("no such column: {c}")))?;
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

fn cmp_values(l: &Value, r: &Value) -> Result<Option<std::cmp::Ordering>> {
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

fn type_mismatch() -> Error {
    Error::Runtime("type mismatch in expression".into())
}

fn div_by_zero() -> Error {
    Error::Runtime("division by zero".into())
}
