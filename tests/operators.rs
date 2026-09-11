use chibidb::ast::{BinOp, Expr};
use chibidb::exec::operator::{Filter, Limit, Project, TableScan};
use chibidb::value::Value;
use chibidb::{Database, Session};

fn seed(db: &mut Database) {
    db.execute_sql("create table t (id int);").unwrap();
    db.execute_sql("insert into t values (1),(2),(3),(4);").unwrap();
}

#[test]
fn scan_filter_project_and_limit() {
    let mut db = Database::open_in_memory().unwrap();
    seed(&mut db);
    let mut session = Session::new();

    let scan = TableScan::new(&db, "t").unwrap();
    let predicate = Expr::Binary(
        BinOp::Gt,
        Box::new(Expr::Column("id".into())),
        Box::new(Expr::Int(1)),
    );
    let filter = Filter::new(Box::new(scan), predicate);
    let project = Project::new(
        Box::new(filter),
        vec![Expr::Column("id".into())],
        vec!["id".into()],
    );
    let mut plan = Limit::new(Box::new(project), 0, Some(2));

    let rows = db.collect_plan(&mut session, &mut plan).unwrap();
    assert_eq!(rows, vec![vec![Value::Int(2)], vec![Value::Int(3)]]);
}

#[test]
fn scan_respects_mvcc_visibility() {
    let mut db = Database::open_in_memory().unwrap();
    seed(&mut db);
    db.execute_sql("delete from t where id = 1;").unwrap();
    let mut session = Session::new();

    let mut scan = TableScan::new(&db, "t").unwrap();
    let rows = db.collect_plan(&mut session, &mut scan).unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::Int(2)], vec![Value::Int(3)], vec![Value::Int(4)]]
    );
}

#[test]
fn limit_offset_skips_rows() {
    let mut db = Database::open_in_memory().unwrap();
    seed(&mut db);
    let mut session = Session::new();

    let scan = TableScan::new(&db, "t").unwrap();
    let mut plan = Limit::new(Box::new(scan), 1, Some(2));
    let rows = db.collect_plan(&mut session, &mut plan).unwrap();
    assert_eq!(rows, vec![vec![Value::Int(2)], vec![Value::Int(3)]]);
}

#[test]
fn project_evaluates_expressions() {
    let mut db = Database::open_in_memory().unwrap();
    seed(&mut db);
    let mut session = Session::new();

    let scan = TableScan::new(&db, "t").unwrap();
    let expr = Expr::Binary(
        BinOp::Add,
        Box::new(Expr::Column("id".into())),
        Box::new(Expr::Int(10)),
    );
    let mut plan = Project::new(Box::new(scan), vec![expr], vec!["plus10".into()]);
    let rows = db.collect_plan(&mut session, &mut plan).unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(11)],
            vec![Value::Int(12)],
            vec![Value::Int(13)],
            vec![Value::Int(14)],
        ]
    );
}
