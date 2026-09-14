use crate::ast::{DeleteStmt, InsertStmt, UpdateStmt};
use crate::catalog::Schema;
use crate::value::Value;
use crate::Result;

use super::operator::{ExecContext, OutputKind, PhysicalOperator};

/// INSERT: performs the inserts in `open` and yields no rows.
pub struct InsertOp {
    stmt: InsertStmt,
    schema: Schema,
    affected: u64,
}

impl InsertOp {
    pub fn new(stmt: InsertStmt) -> Self {
        Self { stmt, schema: Schema::default(), affected: 0 }
    }
}

impl PhysicalOperator for InsertOp {
    fn name(&self) -> &'static str {
        "Insert"
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        vec![
            ("table", self.stmt.table.clone()),
            ("rows", self.stmt.rows.len().to_string()),
        ]
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.affected = super::execute_insert(ctx.db, ctx.trx, &self.stmt)?;
        Ok(())
    }

    fn next(&mut self, _ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        Ok(None)
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn output_kind(&self) -> OutputKind {
        OutputKind::Command
    }

    fn affected_rows(&self) -> Option<u64> {
        Some(self.affected)
    }
}

/// UPDATE: applies matching updates in `open` and yields no rows.
pub struct UpdateOp {
    stmt: UpdateStmt,
    schema: Schema,
    affected: u64,
}

impl UpdateOp {
    pub fn new(stmt: UpdateStmt) -> Self {
        Self { stmt, schema: Schema::default(), affected: 0 }
    }
}

impl PhysicalOperator for UpdateOp {
    fn name(&self) -> &'static str {
        "Update"
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        let assignments: Vec<String> = self
            .stmt
            .assignments
            .iter()
            .map(|(column, value)| format!("{column} = {value}"))
            .collect();
        let mut out = vec![("table", self.stmt.table.clone())];
        out.push(("set", assignments.join(", ")));
        if let Some(selection) = &self.stmt.selection {
            out.push(("where", selection.to_string()));
        }
        out
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.affected = super::execute_update(ctx.db, ctx.trx, &self.stmt)?;
        Ok(())
    }

    fn next(&mut self, _ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        Ok(None)
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn output_kind(&self) -> OutputKind {
        OutputKind::Command
    }

    fn affected_rows(&self) -> Option<u64> {
        Some(self.affected)
    }
}

/// DELETE: marks matching rows in `open` and yields no rows.
pub struct DeleteOp {
    stmt: DeleteStmt,
    schema: Schema,
    affected: u64,
}

impl DeleteOp {
    pub fn new(stmt: DeleteStmt) -> Self {
        Self { stmt, schema: Schema::default(), affected: 0 }
    }
}

impl PhysicalOperator for DeleteOp {
    fn name(&self) -> &'static str {
        "Delete"
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        let mut out = vec![("table", self.stmt.table.clone())];
        if let Some(selection) = &self.stmt.selection {
            out.push(("where", selection.to_string()));
        }
        out
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.affected = super::execute_delete(ctx.db, ctx.trx, &self.stmt)?;
        Ok(())
    }

    fn next(&mut self, _ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        Ok(None)
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn output_kind(&self) -> OutputKind {
        OutputKind::Command
    }

    fn affected_rows(&self) -> Option<u64> {
        Some(self.affected)
    }
}
