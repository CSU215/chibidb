use crate::ast::{
    CreateIndexStmt, CreateTableStmt, CreateViewStmt, DataType, DeleteStmt, DropIndexStmt,
    DropTableStmt, DropViewStmt, Expr, InsertStmt, Stmt, UpdateStmt,
};
use crate::catalog::Schema;
use crate::result::ResultSet;
use crate::storage::codec::decode_record;
use crate::trx::{TrxState, Undo};
use crate::value::Value;
use crate::{Database, Error, Result};

mod aggregate;
mod dml;
mod eval;
pub mod operator;
pub(crate) mod plan;
mod subquery;

pub use eval::eval_const;

use aggregate::cmp_sort_keys;
use eval::EvalCtx;
use plan::execute_explain;
use subquery::{eval_bound, eval_predicate_bound};

pub(crate) fn execute(db: &Database, trx: &mut TrxState, stmt: &Stmt) -> Result<ResultSet> {
    // Schema changes are not isolated from other sessions' open transactions
    // (their undo log points at tables that could vanish), so refuse them.
    if matches!(
        stmt,
        Stmt::CreateTable(_)
            | Stmt::CreateView(_)
            | Stmt::CreateIndex(_)
            | Stmt::DropIndex(_)
            | Stmt::DropTable(_)
            | Stmt::DropView(_)
    ) && !trx.explicit
        && db.has_open_trxs_excluding(trx.id)
    {
        return Err(Error::Runtime("schema is locked by an open transaction".into()));
    }
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
        Stmt::Checkpoint => execute_checkpoint(db, trx),
        Stmt::Vacuum => execute_vacuum(db, trx),
        Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) => {
            Err(Error::Runtime("DML must be executed through the operator plan".into()))
        }
        Stmt::Select(_) => {
            Err(Error::Runtime("select must be executed through the operator plan".into()))
        }
        Stmt::Explain(e) => execute_explain(db, e),
        Stmt::CreateDatabase(_)
        | Stmt::DropDatabase(_)
        | Stmt::Use(_)
        | Stmt::CreateUser(_)
        | Stmt::DropUser(_)
        | Stmt::Grant(_)
        | Stmt::Revoke(_) => {
            Err(Error::Runtime("database statements must be run on an instance".into()))
        }
        Stmt::Trx(_) => Err(Error::Runtime("transaction control handled elsewhere".into())),
    }
}

fn ddl_in_trx(_trx: &TrxState) -> Result<ResultSet> {
    Err(Error::Runtime("DDL inside a transaction is not supported".into()))
}

fn execute_create_index(db: &Database, trx: &mut TrxState, c: &CreateIndexStmt) -> Result<ResultSet> {
    let schema = db.catalog().table(&c.table)?.schema.clone();
    let col_idx = schema
        .index_of(&c.column)
        .ok_or_else(|| Error::Runtime(format!("no such column: {}", c.column)))?;
    let store = db.new_index_heap(&c.name)?;
    let records = db.store_scan_raw(&c.table)?;
    let btree = crate::index::BTree::at(store.file);
    for (rid, rec) in records {
        let (creator, deleter, row) = decode_record(&rec)?;
        if !trx.visible(creator, deleter) {
            continue;
        }
        let key = crate::index::encode_key(&row[col_idx])?;
        btree.insert(&db.pool, &key, rid)?;
    }
    db.catalog_mut().create_index(
        &c.name,
        c.table.clone(),
        c.column.clone(),
        false,
        store,
    )?;
    db.save_catalog()?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn execute_drop_index(db: &Database, d: &DropIndexStmt) -> Result<ResultSet> {
    if db.catalog().index(&d.name).is_some_and(|ix| ix.unique) {
        return Err(Error::Runtime(format!(
            "cannot drop index backing a constraint: {}",
            d.name
        )));
    }
    db.catalog_mut().drop_index(&d.name)?;
    db.save_catalog()?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn execute_drop_table(db: &Database, d: &DropTableStmt) -> Result<ResultSet> {
    db.drop_table(&d.name)?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn execute_create_view(
    db: &Database,
    trx: &mut TrxState,
    c: &CreateViewStmt,
) -> Result<ResultSet> {
    // validate the definition by planning and running its select once
    let stmts = crate::parser::parse(&c.sql)?;
    let Some(Stmt::Select(sel)) = stmts.into_iter().next() else {
        return Err(Error::Runtime("view must be defined by a select".into()));
    };
    let Some(mut plan) = operator::build_select(db, &sel)? else {
        return Err(Error::Runtime("view must be defined by a supported select".into()));
    };
    {
        let mut ctx = operator::ExecContext { db, trx, outer: None };
        plan.open(&mut ctx)?;
        while plan.next(&mut ctx)?.is_some() {}
        plan.close()?;
    }
    db.catalog_mut().create_view(&c.name, c.sql.clone())?;
    db.save_catalog()?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn execute_drop_view(db: &Database, d: &DropViewStmt) -> Result<ResultSet> {
    db.catalog_mut().drop_view(&d.name)?;
    db.save_catalog()?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn execute_checkpoint(db: &Database, trx: &TrxState) -> Result<ResultSet> {
    // the statement's own autocommit transaction does not count, but an
    // explicit one does (truncating the log would drop its future COMMIT)
    if trx.explicit || db.has_open_trxs_excluding(trx.id) {
        return Err(Error::Runtime(
            "cannot checkpoint while transactions are open".into(),
        ));
    }
    // verified no one else is open; skip flush()'s blanket open-trx guard
    // because the statement's own temp transaction is still registered
    db.flush_inner()?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn execute_vacuum(db: &Database, trx: &TrxState) -> Result<ResultSet> {
    // same guard as checkpoint: no transaction may depend on physical state
    if trx.explicit || db.has_open_trxs_excluding(trx.id) {
        return Err(Error::Runtime(
            "cannot vacuum while transactions are open".into(),
        ));
    }
    let purged = db.vacuum()?;
    Ok(ResultSet::Message(format!("VACUUM COMPLETE: {purged} rows purged")))
}

pub(crate) fn execute_update(db: &Database, trx: &mut TrxState, u: &UpdateStmt) -> Result<ResultSet> {
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
    let mut claimed: Vec<(usize, Vec<u8>)> = Vec::new();
    for (rid, rec) in records {
        let (creator, deleter, row) = decode_record(&rec)?;
        if !trx.visible(creator, deleter) {
            continue;
        }
        let matched = match &u.selection {
            Some(sel) => eval_predicate_bound(db, trx, sel, &schema, &row, None)?,
            None => true,
        };
        if !matched {
            continue;
        }
        let mut new_row = row.clone();
        let row_ctx = EvalCtx::row(&schema, &row);
        for (idx, col, dtype, expr) in &assigns {
            let v = eval_bound(db, trx, expr, Some(&row_ctx))?;
            new_row[*idx] = coerce(v, *dtype, col)?;
        }
        check_not_null(&schema, &new_row)?;
        db.check_unique(&u.table, &new_row, Some(rid), trx, &mut claimed)?;
        updates.push((rid, new_row));
    }
    let new_rids = db.store_update_versions(&u.table, &updates, trx.id)?;
    for ((old_rid, new_row), (new_rid, prev_deleter)) in updates.into_iter().zip(new_rids) {
        trx.undo.push(Undo::Update {
            table: u.table.clone(),
            old_rid,
            new_rid,
            new_row,
            prev_deleter,
        });
    }
    Ok(ResultSet::Message("SUCCESS".into()))
}

pub(crate) fn execute_delete(db: &Database, trx: &mut TrxState, d: &DeleteStmt) -> Result<ResultSet> {
    let schema = db.catalog().table(&d.table)?.schema.clone();
    let records = db.store_scan_raw(&d.table)?;
    let mut victims = Vec::new();
    for (rid, rec) in records {
        let (creator, deleter, row) = decode_record(&rec)?;
        if !trx.visible(creator, deleter) {
            continue;
        }
        let matched = match &d.selection {
            Some(sel) => eval_predicate_bound(db, trx, sel, &schema, &row, None)?,
            None => true,
        };
        if matched {
            victims.push(rid);
        }
    }
    let previous = db.store_delete_mark(&d.table, &victims, trx.id)?;
    for (rid, prev_deleter) in victims.into_iter().zip(previous) {
        trx.undo.push(Undo::DeleteMark { table: d.table.clone(), rid, prev_deleter });
    }
    Ok(ResultSet::Message("SUCCESS".into()))
}

/// Resolves the schema positions targeted by an INSERT: either every column
/// or the explicit column list (which may be reordered or partial).
fn insert_targets(schema: &Schema, columns: &Option<Vec<String>>) -> Result<Vec<usize>> {
    let Some(cols) = columns else {
        return Ok((0..schema.columns.len()).collect());
    };
    let mut seen = vec![false; schema.columns.len()];
    let mut targets = Vec::with_capacity(cols.len());
    for c in cols {
        let idx = schema
            .index_of(c)
            .ok_or_else(|| Error::Runtime(format!("no such column: {c}")))?;
        if seen[idx] {
            return Err(Error::Runtime(format!("column specified twice: {c}")));
        }
        seen[idx] = true;
        targets.push(idx);
    }
    Ok(targets)
}

fn check_not_null(schema: &Schema, row: &[Value]) -> Result<()> {
    for (col, v) in schema.columns.iter().zip(row) {
        if col.not_null && matches!(v, Value::Null) {
            return Err(Error::Runtime(format!("column {} cannot be null", col.name)));
        }
    }
    Ok(())
}

pub(crate) fn execute_insert(db: &Database, trx: &mut TrxState, i: &InsertStmt) -> Result<ResultSet> {
    let schema = db.catalog().table(&i.table)?.schema.clone();
    let targets = insert_targets(&schema, &i.columns)?;
    for values in &i.rows {
        if values.len() != targets.len() {
            return Err(Error::Runtime(format!(
                "expected {} values, got {}",
                targets.len(),
                values.len()
            )));
        }
    }
    let mut claimed: Vec<(usize, Vec<u8>)> = Vec::new();
    for values in &i.rows {
        // start from defaults, then overlay the supplied values
        let mut row: Vec<Value> = schema
            .columns
            .iter()
            .map(|c| c.default.clone().unwrap_or(Value::Null))
            .collect();
        for (expr, &idx) in values.iter().zip(&targets) {
            let col = &schema.columns[idx];
            let v = eval_const(expr)?;
            row[idx] = coerce(v, col.dtype, &col.name)?;
        }
        check_not_null(&schema, &row)?;
        db.check_unique(&i.table, &row, None, trx, &mut claimed)?;
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

pub(crate) fn coerce(v: Value, dtype: DataType, col: &str) -> Result<Value> {
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

fn execute_create_table(db: &Database, c: &CreateTableStmt) -> Result<ResultSet> {
    let mut columns = Vec::with_capacity(c.columns.len());
    for cd in &c.columns {
        let default = match &cd.default {
            Some(e) => Some(coerce(eval_const(e)?, cd.dtype, &cd.name)?),
            None => None,
        };
        if cd.not_null && matches!(default, Some(Value::Null)) {
            return Err(Error::Runtime(format!(
                "column {} cannot have a NULL default",
                cd.name
            )));
        }
        columns.push(crate::catalog::ColumnDesc {
            owner: None,
            name: cd.name.clone(),
            dtype: cd.dtype,
            not_null: cd.not_null,
            primary_key: cd.primary_key,
            unique: cd.unique,
            default,
        });
    }
    let schema = Schema { columns };
    let kind = db.default_engine();
    let (heap, engine) = db.new_table_storage(kind)?;
    db.catalog_mut().create_table(&c.name, schema, heap, kind, engine)?;
    // PRIMARY KEY / UNIQUE get a constraint-backed unique index; the table is
    // empty here, so there is nothing to populate.
    for cd in &c.columns {
        if cd.primary_key || cd.unique {
            let name = unique_index_name(&c.name, &cd.name);
            let store = db.new_index_heap(&name)?;
            db.catalog_mut().create_index(&name, c.name.clone(), cd.name.clone(), true, store)?;
        }
    }
    db.save_catalog()?;
    Ok(ResultSet::Message("SUCCESS".into()))
}

fn unique_index_name(table: &str, column: &str) -> String {
    format!("__unique_{table}_{column}")
}

/// ORDER BY over an already-projected result set: column references resolve
/// against the output column names.
pub(crate) fn sort_projected(
    db: &Database,
    trx: &mut TrxState,
    outer: Option<&EvalCtx>,
    columns: &[String],
    rows: &mut Vec<Vec<Value>>,
    order_by: &[(Expr, bool)],
) -> Result<()> {
    let schema = Schema {
        columns: columns
            .iter()
            .map(|c| crate::catalog::ColumnDesc::plain(None, c.clone(), DataType::Text))
            .collect(),
    };
    let mut pairs: Vec<(Vec<Value>, Vec<Value>)> = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        let mut ctx = EvalCtx::row(&schema, &row);
        ctx.parent = outer;
        let mut keys = Vec::with_capacity(order_by.len());
        for (e, _) in order_by {
            keys.push(eval_bound(db, trx, e, Some(&ctx))?);
        }
        pairs.push((row, keys));
    }
    pairs.sort_by(|(_, ka), (_, kb)| cmp_sort_keys(ka, kb, order_by));
    rows.extend(pairs.into_iter().map(|(r, _)| r));
    Ok(())
}


