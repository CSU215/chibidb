//! Resolved DML commands, shared by the logical plan and the command operators.
//!
//! Translation binds column names to schema positions here, so the physical
//! operators carry plain data instead of the original AST statement.

use crate::sql::ast::{DeleteStmt, Expr, InsertStmt, UpdateStmt};
use crate::{Database, Error, Result};

/// `INSERT`: the table, the target column positions, and the value rows.
#[derive(Clone)]
pub(crate) struct InsertCommand {
    pub(crate) table: String,
    pub(crate) targets: Vec<usize>,
    pub(crate) rows: Vec<Vec<Expr>>,
}

impl InsertCommand {
    pub(crate) fn resolve(db: &Database, stmt: &InsertStmt) -> Result<Self> {
        let schema = db.catalog().table(&stmt.table)?.schema.clone();
        let targets = insert_targets(&schema, &stmt.columns)?;
        for values in &stmt.rows {
            if values.len() != targets.len() {
                return Err(Error::Runtime(format!(
                    "expected {} values, got {}",
                    targets.len(),
                    values.len()
                )));
            }
        }
        Ok(Self { table: stmt.table.clone(), targets, rows: stmt.rows.clone() })
    }
}

/// `UPDATE`: the table, the `(column position, value)` assignments, and the
/// optional row selection.
#[derive(Clone)]
pub(crate) struct UpdateCommand {
    pub(crate) table: String,
    pub(crate) assignments: Vec<(usize, Expr)>,
    pub(crate) selection: Option<Expr>,
}

impl UpdateCommand {
    pub(crate) fn resolve(db: &Database, stmt: &UpdateStmt) -> Result<Self> {
        let schema = db.catalog().table(&stmt.table)?.schema.clone();
        let mut assignments = Vec::with_capacity(stmt.assignments.len());
        for (col, expr) in &stmt.assignments {
            let idx = schema
                .index_of(col)
                .ok_or_else(|| Error::Runtime(format!("no such column: {col}")))?;
            assignments.push((idx, expr.clone()));
        }
        Ok(Self {
            table: stmt.table.clone(),
            assignments,
            selection: stmt.selection.clone(),
        })
    }
}

/// `DELETE`: the table and the optional row selection.
#[derive(Clone)]
pub(crate) struct DeleteCommand {
    pub(crate) table: String,
    pub(crate) selection: Option<Expr>,
}

impl DeleteCommand {
    pub(crate) fn resolve(_db: &Database, stmt: &DeleteStmt) -> Result<Self> {
        Ok(Self { table: stmt.table.clone(), selection: stmt.selection.clone() })
    }
}

/// Resolves the schema positions targeted by an INSERT: either every column or
/// the explicit column list (which may be reordered or partial).
fn insert_targets(schema: &crate::catalog::Schema, columns: &Option<Vec<String>>) -> Result<Vec<usize>> {
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
