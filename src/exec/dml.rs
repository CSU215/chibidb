use crate::ast::{DeleteStmt, InsertStmt, UpdateStmt};
use crate::catalog::Schema;
use crate::value::Value;
use crate::Result;

use super::operator::{ExecContext, OutputKind, PhysicalOperator};

/// INSERT: performs the inserts in `open` and yields no rows.
pub struct InsertOp {
    stmt: InsertStmt,
    schema: Schema,
}

impl InsertOp {
    pub fn new(stmt: InsertStmt) -> Self {
        Self { stmt, schema: Schema::default() }
    }
}

impl PhysicalOperator for InsertOp {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        super::execute_insert(ctx.db, ctx.trx, &self.stmt)?;
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
}

/// UPDATE: applies matching updates in `open` and yields no rows.
pub struct UpdateOp {
    stmt: UpdateStmt,
    schema: Schema,
}

impl UpdateOp {
    pub fn new(stmt: UpdateStmt) -> Self {
        Self { stmt, schema: Schema::default() }
    }
}

impl PhysicalOperator for UpdateOp {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        super::execute_update(ctx.db, ctx.trx, &self.stmt)?;
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
}

/// DELETE: marks matching rows in `open` and yields no rows.
pub struct DeleteOp {
    stmt: DeleteStmt,
    schema: Schema,
}

impl DeleteOp {
    pub fn new(stmt: DeleteStmt) -> Self {
        Self { stmt, schema: Schema::default() }
    }
}

impl PhysicalOperator for DeleteOp {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        super::execute_delete(ctx.db, ctx.trx, &self.stmt)?;
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
}
