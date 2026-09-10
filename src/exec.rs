use crate::ast::{
    BinOp, CreateIndexStmt, CreateTableStmt, CreateViewStmt, DataType, DeleteStmt, DropIndexStmt,
    DropTableStmt, DropViewStmt, ExplainStmt, Expr, InsertStmt, Limit, SelectItem, SelectStmt,
    Stmt, TableRef, UnOp, UpdateStmt,
};
use crate::catalog::Schema;
use crate::index::{encode_key, BTree, Bound};
use crate::result::ResultSet;
use crate::storage::codec::decode_record;
use crate::storage::Rid;
use crate::trx::{TrxState, Undo};
use crate::value::Value;
use crate::{Database, Error, Result};

pub(crate) fn execute(db: &mut Database, trx: &mut TrxState, stmt: &Stmt) -> Result<ResultSet> {
    match stmt {
        Stmt::CreateTable(c) if trx.explicit => ddl_in_trx(trx),
        Stmt::CreateTable(c) => execute_create_table(db, c),
        Stmt::CreateView(c) if trx.explicit => ddl_in_trx(trx),
        Stmt::CreateView(c) => execute_create_view(db, trx, c),
        Stmt::CreateIndex(c) if trx.explicit => ddl_in_trx(trx),
        Stmt::CreateIndex(c) => execute_create_index(db, trx, c),
        Stmt::DropIndex(d) if trx.explicit => ddl_in_trx(trx),
        Stmt::DropIndex(d) => execute_drop_index(db, d),
        Stmt::DropTable(d) if trx.explicit => ddl_in_trx(trx),
        Stmt::DropTable(d) => execute_drop_table(db, d),
        Stmt::DropView(d) if trx.explicit => ddl_in_trx(trx),
        Stmt::DropView(d) => execute_drop_view(db, d),
        Stmt::Insert(i) => execute_insert(db, trx, i),
        Stmt::Select(s) => execute_select(db, trx, s),
        Stmt::Delete(d) => execute_delete(db, trx, d),
        Stmt::Update(u) => execute_update(db, trx, u),
        Stmt::Explain(e) => execute_explain(db, e),
        Stmt::Trx(_) => Err(Error::Runtime("transaction control handled elsewhere".into())),
    }
}

fn ddl_in_trx(_trx: &TrxState) -> Result<ResultSet> {
    Err(Error::Runtime("DDL inside a transaction is not supported".into()))
}

/// Decodes versioned records and keeps only rows visible to `trx`.
fn decode_visible(
    records: Vec<(Rid, Vec<u8>)>,
    trx: &TrxState,
) -> Result<Vec<Vec<Value>>> {
    let mut out = Vec::new();
    for (_, rec) in records {
        let (creator, deleter, row) = decode_record(&rec)?;
        if trx.visible(creator, deleter) {
            out.push(row);
        }
    }
    Ok(out)
}

fn execute_explain(db: &mut Database, e: &ExplainStmt) -> Result<ResultSet> {
    match &*e.stmt {
        Stmt::Select(s) => Ok(ResultSet::Message(plan_select(db, s)?)),
        _ => Err(Error::Runtime("explain supports select only".into())),
    }
}

fn plan_select(db: &mut Database, s: &SelectStmt) -> Result<String> {
    if s.from.is_empty() {
        return Ok("ConstantSelect -> Project".into());
    }
    if s.from.len() > 1 {
        return Ok(format!(
            "NestedLoopJoin(tables={}) -> Filter -> Project",
            s.from.len()
        ));
    }
    match find_sargable(db, &s.from[0].name, s.selection.as_ref())? {
        Some(sarg) => Ok(format!(
            "IndexScan(index={}, table={}, where {} {}) -> Filter -> Project",
            sarg.index, s.from[0].name, sarg.column, sarg.op
        )),
        None => Ok(format!("FullScan(table={}) -> Filter -> Project", s.from[0].name)),
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

pub(crate) fn expr_has_column(expr: &Expr) -> bool {
    match expr {
        Expr::Column(_) | Expr::QualifiedColumn(..) => true,
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
    // views have no indexes, so no access path choice applies
    if db.catalog().view(table).is_some() {
        return Ok(None);
    }
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

fn execute_create_index(db: &mut Database, trx: &mut TrxState, c: &CreateIndexStmt) -> Result<ResultSet> {
    let schema = db.catalog().table(&c.table)?.schema.clone();
    let col_idx = schema
        .index_of(&c.column)
        .ok_or_else(|| Error::Runtime(format!("no such column: {}", c.column)))?;
    let store = db.new_index_heap(&c.name)?;
    let records = db.store_scan_raw(&c.table)?;
    let btree = BTree::at(store.file);
    for (rid, rec) in records {
        let (creator, deleter, row) = decode_record(&rec)?;
        if !trx.visible(creator, deleter) {
            continue;
        }
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

fn execute_drop_table(db: &mut Database, d: &DropTableStmt) -> Result<ResultSet> {
    db.drop_table(&d.name)?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn execute_create_view(
    db: &mut Database,
    trx: &mut TrxState,
    c: &CreateViewStmt,
) -> Result<ResultSet> {
    // validate the definition by executing its select once (read-only)
    let stmts = crate::parser::parse(&c.sql)?;
    let Some(Stmt::Select(sel)) = stmts.into_iter().next() else {
        return Err(Error::Runtime("view must be defined by a select".into()));
    };
    execute_select(db, trx, &sel)?;
    db.catalog_mut().create_view(&c.name, c.sql.clone())?;
    db.save_catalog()?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn execute_drop_view(db: &mut Database, d: &DropViewStmt) -> Result<ResultSet> {
    db.catalog_mut().drop_view(&d.name)?;
    db.save_catalog()?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn execute_update(db: &mut Database, trx: &mut TrxState, u: &UpdateStmt) -> Result<ResultSet> {
    let schema = db.catalog().table(&u.table)?.schema.clone();
    let mut assigns = Vec::new();
    for (col, expr) in &u.assignments {
        let idx = schema
            .index_of(col)
            .ok_or_else(|| Error::Runtime(format!("no such column: {col}")))?;
        assigns.push((idx, col.clone(), schema.columns[idx].dtype, expr));
    }
    let records = db.store_scan_raw(&u.table)?;
    let mut updates = Vec::new();
    for (rid, rec) in records {
        let (creator, deleter, row) = decode_record(&rec)?;
        if !trx.visible(creator, deleter) {
            continue;
        }
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
        updates.push((rid, new_row));
    }
    let new_rids = db.store_update_versions(&u.table, &updates, trx.id)?;
    for ((old_rid, new_row), new_rid) in updates.into_iter().zip(new_rids) {
        trx.undo.push(Undo::Update {
            table: u.table.clone(),
            old_rid,
            new_rid,
            new_row,
        });
    }
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn execute_delete(db: &mut Database, trx: &mut TrxState, d: &DeleteStmt) -> Result<ResultSet> {
    let schema = db.catalog().table(&d.table)?.schema.clone();
    let records = db.store_scan_raw(&d.table)?;
    let mut victims = Vec::new();
    for (rid, rec) in records {
        let (creator, deleter, row) = decode_record(&rec)?;
        if !trx.visible(creator, deleter) {
            continue;
        }
        let matched = match &d.selection {
            Some(sel) => eval_predicate(sel, &schema, &row)?,
            None => true,
        };
        if matched {
            victims.push(rid);
        }
    }
    db.store_delete_mark(&d.table, &victims, trx.id)?;
    for rid in victims {
        trx.undo.push(Undo::DeleteMark { table: d.table.clone(), rid });
    }
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

fn execute_insert(db: &mut Database, trx: &mut TrxState, i: &InsertStmt) -> Result<ResultSet> {
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
        if encoded.len() + 16 > crate::storage::PAGE_SIZE {
            return Err(Error::Runtime(format!(
                "record too large ({} bytes does not fit in a page)",
                encoded.len()
            )));
        }
        let rid = db.store_insert(&i.table, row.clone(), trx.id)?;
        trx.undo.push(Undo::Insert { table: i.table.clone(), rid, row });
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
                owner: None,
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

/// Materializes every uncorrelated subquery in a select into literal
/// values before row evaluation, so `eval` stays context-free. Subqueries
/// inside subqueries recurse naturally (each `execute_select` lifts again).
fn lift_subqueries(db: &mut Database, trx: &mut TrxState, s: &SelectStmt) -> Result<SelectStmt> {
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
    Ok(SelectStmt { items, from: s.from.clone(), on, selection, group_by, having, order_by, limit: s.limit.clone() })
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
    match execute_select(db, trx, &sel)? {
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

fn execute_select(db: &mut Database, trx: &mut TrxState, s: &SelectStmt) -> Result<ResultSet> {
    let s = &lift_subqueries(db, trx, s)?;
    if s.from.is_empty() {
        let mut columns = Vec::new();
        let mut row = Vec::new();
        for item in &s.items {
            match item {
                SelectItem::Expr(e) => {
                    columns.push(e.to_string());
                    row.push(eval_const(e)?);
                }
                SelectItem::Aliased(e, alias) => {
                    columns.push(alias.clone());
                    row.push(eval_const(e)?);
                }
                SelectItem::Star => {
                    return Err(Error::Runtime("select * requires from".into()))
                }
            }
        }
        return Ok(ResultSet::Rows { columns, rows: vec![row] });
    }
    // nested-loop inner join over all FROM tables (comma list and JOIN..ON alike)
    let mut schema = Schema::default();
    let mut rows: Vec<Vec<Value>> = vec![vec![]];
    for (i, tref) in s.from.iter().enumerate() {
        let owner = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
        let (columns, visible) = from_source(db, trx, tref)?;
        for col in columns {
            schema.columns.push(crate::catalog::ColumnDesc {
                owner: Some(owner.clone()),
                name: col.name,
                dtype: col.dtype,
            });
        }
        let mut combined = Vec::with_capacity(rows.len() * visible.len().max(1));
        for left in rows {
            for right in &visible {
                let mut row = left.clone();
                row.extend(right.iter().cloned());
                combined.push(row);
            }
        }
        rows = combined;
        if i >= 1 {
            // on[i-1] joins the newly added table with everything before it
            if let Some(cond) = s.on.get(i - 1) {
                let mut kept = Vec::with_capacity(rows.len());
                for row in rows {
                    if eval_predicate(cond, &schema, &row)? {
                        kept.push(row);
                    }
                }
                rows = kept;
            }
        }
    }
    let mut headers = Vec::new();
    let mut exprs = Vec::new();
    for item in &s.items {
        match item {
            SelectItem::Star => {
                for col in &schema.columns {
                    headers.push(col.name.clone());
                    // qualified reference avoids ambiguity when column names repeat
                    match &col.owner {
                        Some(owner) => exprs.push(Expr::QualifiedColumn(owner.clone(), col.name.clone())),
                        None => exprs.push(Expr::Column(col.name.clone())),
                    }
                }
            }
            SelectItem::Expr(e) => {
                headers.push(e.to_string());
                exprs.push(e.clone());
            }
            SelectItem::Aliased(e, alias) => {
                headers.push(alias.clone());
                exprs.push(e.clone());
            }
        }
    }
    // index scan only helps single-table scans
    let mut source_rows: Vec<Vec<Value>> = rows;
    if s.from.len() == 1
        && let Some(sarg) = find_sargable(db, &s.from[0].name, s.selection.as_ref())? {
            let lit_val = eval_const(&sarg.lit)?;
            let coerced = coerce(lit_val, sarg.dtype, &sarg.column)?;
            let key = encode_key(&coerced)?;
            let ix_file = db
                .catalog()
                .indexes_for(&s.from[0].name)
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
            source_rows = decode_visible(db.store_get_records(&s.from[0].name, &rids)?, trx)?;
        }
    let mut filtered: Vec<Vec<Value>> = Vec::new();
    for row in source_rows {
        if let Some(sel) = &s.selection
            && !eval_predicate(sel, &schema, &row)? {
                continue;
            }
        filtered.push(row);
    }
    let has_aggregate = s
        .items
        .iter()
        .any(|it| matches!(it, SelectItem::Expr(e) | SelectItem::Aliased(e, _) if expr_has_aggregate(e)))
        || s.having.as_ref().is_some_and(expr_has_aggregate);

    if !s.group_by.is_empty() || has_aggregate {
        return execute_grouped_select(&schema, s, filtered, headers, exprs);
    }
    if !s.order_by.is_empty() {
        sort_rows(&schema, &mut filtered, &s.order_by, &s.items)?;
    }
    let mut out_rows = Vec::new();
    for row in filtered {
        let mut out_row = Vec::with_capacity(exprs.len());
        for e in &exprs {
            out_row.push(eval(e, Some(&EvalCtx::Row(&schema, &row)))?);
        }
        out_rows.push(out_row);
    }
    apply_limit(&mut out_rows, &s.limit)?;
    Ok(ResultSet::Rows { columns: headers, rows: out_rows })
}

fn apply_limit(out_rows: &mut Vec<Vec<Value>>, limit: &Option<Limit>) -> Result<()> {
    let Some(limit) = limit else {
        return Ok(());
    };
    let count = match eval_const(&limit.count)? {
        Value::Int(n) if n >= 0 => n as usize,
        _ => return Err(Error::Runtime("limit count must be a non-negative integer".into())),
    };
    let offset = match &limit.offset {
        Some(e) => match eval_const(e)? {
            Value::Int(n) if n >= 0 => n as usize,
            _ => {
                return Err(Error::Runtime(
                    "limit offset must be a non-negative integer".into(),
                ))
            }
        },
        None => 0,
    };
    *out_rows = out_rows.iter().skip(offset).take(count).cloned().collect();
    Ok(())
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
            if let SelectItem::Expr(e) | SelectItem::Aliased(e, _) = it
                && expr_has_column(e) {
                    return Err(Error::Runtime(
                        "column must appear in group by or aggregate".into(),
                    ));
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
    let mut surviving: Vec<Vec<Vec<Value>>> = Vec::new();
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
        surviving.push(group_rows);
    }
    if !s.order_by.is_empty() {
        sort_groups(schema, &mut surviving, &s.order_by, &s.items)?;
    }
    let mut out_rows = Vec::new();
    for group_rows in surviving {
        let mut out_row = Vec::with_capacity(exprs.len());
        for e in &exprs {
            out_row.push(eval(e, Some(&EvalCtx::Group(schema, &group_rows)))?);
        }
        out_rows.push(out_row);
    }
    apply_limit(&mut out_rows, &s.limit)?;
    Ok(ResultSet::Rows { columns: headers, rows: out_rows })
}

fn select_aliases(items: &[SelectItem]) -> Vec<(String, Expr)> {
    items
        .iter()
        .filter_map(|it| match it {
            SelectItem::Aliased(e, alias) => Some((alias.clone(), e.clone())),
            _ => None,
        })
        .collect()
}

/// ORDER BY may reference output aliases (e.g. `count(*) as total`).
fn resolve_order_expr<'a>(expr: &'a Expr, aliases: &'a [(String, Expr)]) -> &'a Expr {
    match expr {
        Expr::Column(c) => aliases
            .iter()
            .find(|(a, _)| a == c)
            .map(|(_, e)| e)
            .unwrap_or(expr),
        _ => expr,
    }
}

fn eval_sort_keys(
    ctx: EvalCtx,
    order_by: &[(Expr, bool)],
    aliases: &[(String, Expr)],
) -> Result<Vec<Value>> {
    order_by
        .iter()
        .map(|(e, _)| eval(resolve_order_expr(e, aliases), Some(&ctx)))
        .collect()
}

fn cmp_sort_keys(a: &[Value], b: &[Value], order_by: &[(Expr, bool)]) -> std::cmp::Ordering {
    for ((_, desc), (va, vb)) in order_by.iter().zip(a.iter().zip(b.iter())) {
        // nulls sort as smallest: first on asc, last on desc
        let ord = match (va, vb) {
            (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
            (Value::Null, _) => std::cmp::Ordering::Less,
            (_, Value::Null) => std::cmp::Ordering::Greater,
            _ => cmp_values(va, vb).ok().flatten().unwrap_or(std::cmp::Ordering::Equal),
        };
        let ord = if *desc { ord.reverse() } else { ord };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

fn sort_rows(
    schema: &Schema,
    rows: &mut Vec<Vec<Value>>,
    order_by: &[(Expr, bool)],
    items: &[SelectItem],
) -> Result<()> {
    let aliases = select_aliases(items);
    let mut pairs: Vec<(Vec<Value>, Vec<Value>)> = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        let keys = eval_sort_keys(EvalCtx::Row(schema, &row), order_by, &aliases)?;
        pairs.push((row, keys));
    }
    pairs.sort_by(|(_, ka), (_, kb)| cmp_sort_keys(ka, kb, order_by));
    rows.extend(pairs.into_iter().map(|(r, _)| r));
    Ok(())
}

fn sort_groups(
    schema: &Schema,
    groups: &mut Vec<Vec<Vec<Value>>>,
    order_by: &[(Expr, bool)],
    items: &[SelectItem],
) -> Result<()> {
    let aliases = select_aliases(items);
    let mut pairs: Vec<(Vec<Vec<Value>>, Vec<Value>)> = Vec::with_capacity(groups.len());
    for group in groups.drain(..) {
        let keys = eval_sort_keys(EvalCtx::Group(schema, &group), order_by, &aliases)?;
        pairs.push((group, keys));
    }
    pairs.sort_by(|(_, ka), (_, kb)| cmp_sort_keys(ka, kb, order_by));
    groups.extend(pairs.into_iter().map(|(g, _)| g));
    Ok(())
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
        Expr::Value(v) => Ok(v.clone()),
        Expr::InSubquery { .. } | Expr::Exists { .. } | Expr::ScalarSubquery(_) => {
            Err(Error::Runtime("subqueries are materialized before evaluation".into()))
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
