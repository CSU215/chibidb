use crate::ast::Stmt;
use crate::result::ResultSet;
use crate::trx::Session;
use crate::{Database, Result};

/// Per-statement state that stages read and fill in as it flows through the
/// pipeline. Later stages will add bound statements and plans; today the only
/// artifact is the result.
pub struct SqlEvent<'a> {
    pub stmt: &'a Stmt,
    pub result: Option<ResultSet>,
}

impl<'a> SqlEvent<'a> {
    pub fn new(stmt: &'a Stmt) -> Self {
        Self { stmt, result: None }
    }
}

/// One processing stage. Stages are stateless and the database is passed in
/// per call, so a pipeline never has to borrow the database it runs against.
pub trait Stage {
    fn handle(
        &self,
        db: &mut Database,
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
        db: &mut Database,
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
        db: &mut Database,
        session: &mut Session,
        event: &mut SqlEvent<'_>,
    ) -> Result<()> {
        let trx = session.trx();
        event.result = Some(crate::exec::execute(db, trx, event.stmt)?);
        Ok(())
    }
}
