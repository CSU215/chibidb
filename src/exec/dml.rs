use crate::catalog::Schema;
use crate::value::Value;
use crate::Result;

use super::command::{DeleteCommand, InsertCommand, UpdateCommand};
use super::operator::{ExecContext, OutputKind, PhysicalOperator};

/// INSERT: performs the inserts in `open` and yields no rows.
pub struct InsertOp {
    cmd: InsertCommand,
    schema: Schema,
    affected: u64,
}

impl InsertOp {
    pub fn new(cmd: InsertCommand) -> Self {
        Self { cmd, schema: Schema::default(), affected: 0 }
    }
}

impl PhysicalOperator for InsertOp {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn label(&self) -> String {
        format!("Insert {}", self.cmd.table)
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.affected = super::execute_insert(ctx.db, ctx.trx, &self.cmd)?;
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
    cmd: UpdateCommand,
    schema: Schema,
    affected: u64,
}

impl UpdateOp {
    pub fn new(cmd: UpdateCommand) -> Self {
        Self { cmd, schema: Schema::default(), affected: 0 }
    }
}

impl PhysicalOperator for UpdateOp {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn label(&self) -> String {
        format!("Update {}", self.cmd.table)
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.affected = super::execute_update(ctx.db, ctx.trx, &self.cmd)?;
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
    cmd: DeleteCommand,
    schema: Schema,
    affected: u64,
}

impl DeleteOp {
    pub fn new(cmd: DeleteCommand) -> Self {
        Self { cmd, schema: Schema::default(), affected: 0 }
    }
}

impl PhysicalOperator for DeleteOp {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn label(&self) -> String {
        format!("Delete {}", self.cmd.table)
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.affected = super::execute_delete(ctx.db, ctx.trx, &self.cmd)?;
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
