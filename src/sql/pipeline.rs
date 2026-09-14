use crate::ast::Stmt;
use crate::exec::operator::PhysicalOperator;
use crate::result::ResultSet;
use crate::trx::Session;
use crate::{Database, Error, Result};

/// Per-statement state that stages read and fill in as it flows through the
/// pipeline.
pub struct SqlEvent<'a> {
    pub stmt: &'a Stmt,
    /// Tables referenced by the statement, filled by [`ResolveStage`].
    pub tables: Vec<String>,
    /// Access-path description for a SELECT, filled by [`OptimizeStage`].
    pub plan: Option<String>,
    /// Physical operator tree, when the operators cover the statement.
    pub physical: Option<Box<dyn PhysicalOperator>>,
    pub result: Option<ResultSet>,
}

impl<'a> SqlEvent<'a> {
    pub fn new(stmt: &'a Stmt) -> Self {
        Self { stmt, tables: Vec::new(), plan: None, physical: None, result: None }
    }
}

/// One processing stage. Stages are stateless and the database is passed in
/// per call, so a pipeline never has to borrow the database it runs against.
pub trait Stage {
    fn handle(
        &self,
        db: &Database,
        session: &mut Session,
        event: &mut SqlEvent<'_>,
    ) -> Result<()>;
}

/// Runs stages in order over one statement.
pub struct Pipeline {
    stages: Vec<Box<dyn Stage>>,
}

impl Pipeline {
    pub fn new(stages: Vec<Box<dyn Stage>>) -> Self {
        Self { stages }
    }

    pub fn run(
        &self,
        db: &Database,
        session: &mut Session,
        event: &mut SqlEvent<'_>,
    ) -> Result<()> {
        for stage in &self.stages {
            stage.handle(db, session, event)?;
        }
        Ok(())
    }
}

/// Executes the statement against the session's active transaction.
pub struct ExecuteStage;

impl Stage for ExecuteStage {
    fn handle(
        &self,
        db: &Database,
        session: &mut Session,
        event: &mut SqlEvent<'_>,
    ) -> Result<()> {
        let result = if let Some(plan) = event.physical.as_mut() {
            let kind = plan.output_kind();
            let columns: Vec<String> =
                plan.schema().columns.iter().map(|c| c.name.clone()).collect();
            let rows = db.collect_plan(session, plan.as_mut())?;
            match kind {
                crate::exec::operator::OutputKind::Command => match plan.affected_rows() {
                    Some(n) => ResultSet::Affected(n),
                    None => ResultSet::Message("SUCCESS".into()),
                },
                crate::exec::operator::OutputKind::Rows => ResultSet::Rows { columns, rows },
            }
        } else {
            crate::exec::execute(db, session.trx(), event.stmt)?
        };
        event.result = Some(result);
        Ok(())
    }
}

/// Resolves the statement's table references against the catalog, failing
/// early when a referenced table or view does not exist.
pub struct ResolveStage;

impl Stage for ResolveStage {
    fn handle(
        &self,
        db: &Database,
        _session: &mut Session,
        event: &mut SqlEvent<'_>,
    ) -> Result<()> {
        event.tables = referenced_tables(event.stmt);
        for table in &event.tables {
            let known = db.table_exists(table) || db.catalog().view(table).is_some();
            if !known {
                return Err(Error::Runtime(format!("no such table: {table}")));
            }
        }
        Ok(())
    }
}

/// Chooses the access path for a SELECT and records a human-readable plan.
/// The executor re-derives the same path today; later phases will consume it.
pub struct OptimizeStage;

impl Stage for OptimizeStage {
    fn handle(
        &self,
        db: &Database,
        _session: &mut Session,
        event: &mut SqlEvent<'_>,
    ) -> Result<()> {
        if let Stmt::Select(select) = event.stmt {
            event.plan = Some(crate::exec::plan::plan_select(db, select)?);
        }
        event.physical = crate::exec::operator::build_statement(db, event.stmt)?;
        Ok(())
    }
}

/// Top-level tables a statement reads or writes, used for resolution.
fn referenced_tables(stmt: &Stmt) -> Vec<String> {
    match stmt {
        Stmt::Select(s) => s.from.iter().map(|t| t.name.clone()).collect(),
        Stmt::ShowColumns(c) => vec![c.table.clone()],
        Stmt::Insert(i) => vec![i.table.clone()],
        Stmt::Update(u) => vec![u.table.clone()],
        Stmt::Delete(d) => vec![d.table.clone()],
        _ => Vec::new(),
    }
}
