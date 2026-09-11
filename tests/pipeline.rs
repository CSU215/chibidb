use chibidb::pipeline::{OptimizeStage, Pipeline, ResolveStage, SqlEvent, Stage};
use chibidb::{Database, Error, Result, ResultSet, Session};

struct Marker(&'static str);

impl Stage for Marker {
    fn handle(
        &self,
        _db: &mut Database,
        _session: &mut Session,
        event: &mut SqlEvent<'_>,
    ) -> Result<()> {
        event.result = Some(ResultSet::Message(self.0.to_string()));
        Ok(())
    }
}

struct Boom;

impl Stage for Boom {
    fn handle(
        &self,
        _db: &mut Database,
        _session: &mut Session,
        _event: &mut SqlEvent<'_>,
    ) -> Result<()> {
        Err(Error::Runtime("boom".into()))
    }
}

fn one_stmt(sql: &str) -> chibidb::ast::Stmt {
    chibidb::parser::parse(sql).unwrap().remove(0)
}

#[test]
fn pipeline_runs_stages_in_order() {
    let mut db = Database::open_in_memory().unwrap();
    let mut session = Session::new();
    let stmt = one_stmt("select 1;");

    let mut event = SqlEvent::new(&stmt);
    let pipeline = Pipeline::new(vec![Box::new(Marker("first")), Box::new(Marker("second"))]);
    pipeline.run(&mut db, &mut session, &mut event).unwrap();

    assert_eq!(event.result, Some(ResultSet::Message("second".into())));
}

#[test]
fn pipeline_stops_on_stage_error() {
    let mut db = Database::open_in_memory().unwrap();
    let mut session = Session::new();
    let stmt = one_stmt("select 1;");

    let mut event = SqlEvent::new(&stmt);
    let pipeline = Pipeline::new(vec![Box::new(Boom), Box::new(Marker("never"))]);
    assert!(pipeline.run(&mut db, &mut session, &mut event).is_err());
    assert_eq!(event.result, None);
}

#[test]
fn resolve_stage_validates_table_references() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int);").unwrap();
    let mut session = Session::new();
    let pipeline = Pipeline::new(vec![Box::new(ResolveStage)]);

    let good = one_stmt("select * from t;");
    let mut event = SqlEvent::new(&good);
    pipeline.run(&mut db, &mut session, &mut event).unwrap();
    assert_eq!(event.tables, ["t"]);

    let missing = one_stmt("select * from missing;");
    let mut event = SqlEvent::new(&missing);
    assert!(pipeline.run(&mut db, &mut session, &mut event).is_err());
}

#[test]
fn optimize_stage_records_an_index_plan() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int);").unwrap();
    db.execute_sql("create index idx on t (id);").unwrap();
    let mut session = Session::new();

    let pipeline = Pipeline::new(vec![Box::new(OptimizeStage)]);
    let stmt = one_stmt("select * from t where id = 1;");
    let mut event = SqlEvent::new(&stmt);
    pipeline.run(&mut db, &mut session, &mut event).unwrap();
    let plan = event.plan.expect("plan was recorded");
    assert!(plan.contains("IndexScan"), "{plan}");
}
