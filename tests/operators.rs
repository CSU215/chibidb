use chibidb::ast::{BinOp, Expr, Stmt};
use chibidb::exec::operator::{build_select, Filter, Limit, Project, TableScan};
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

fn parse_select(sql: &str) -> Box<chibidb::ast::SelectStmt> {
    match chibidb::parser::parse(sql).unwrap().remove(0) {
        Stmt::Select(s) => s,
        other => panic!("expected select, got {other:?}"),
    }
}

#[test]
fn simple_select_builds_an_operator_plan() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int, tag int);").unwrap();
    db.execute_sql("create index idx on t (id);").unwrap();

    let select = parse_select("select id from t where id = 1 limit 1;");
    assert!(build_select(&mut db, &select).unwrap().is_some());
}

#[test]
fn constant_and_distinct_selects_build_plans() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int);").unwrap();

    for sql in [
        "select 1 + 2;",
        "select distinct id from t;",
        "select id from t order by id;",
        "select count(*) from t;",
        "select id, count(*) from t group by id;",
    ] {
        let select = parse_select(sql);
        assert!(build_select(&mut db, &select).unwrap().is_some(), "{sql}");
    }
}

#[test]
fn joins_build_an_operator_plan() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute_sql("create table a (id int);").unwrap();
    db.execute_sql("create table b (id int);").unwrap();

    for sql in [
        "select * from a, b;",
        "select * from a join b on a.id = b.id;",
        "select * from a left join b on a.id = b.id;",
        "select * from a right join b on a.id = b.id;",
    ] {
        let select = parse_select(sql);
        assert!(build_select(&mut db, &select).unwrap().is_some(), "{sql}");
    }
}

#[test]
fn representative_selects_all_build_plans() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int, tag int);").unwrap();
    db.execute_sql("create index idx on t (id);").unwrap();

    for sql in [
        "select 1;",
        "select id from t;",
        "select id, tag from t where id = 1;",
        "select distinct tag from t;",
        "select id from t order by id desc;",
        "select count(*), sum(id) from t;",
        "select tag, count(*) from t group by tag having count(*) > 1;",
        "select id from t order by id limit 2 offset 1;",
        "select a.id from t a, t b where a.id = b.id;",
        "select id from t union select id from t;",
        "select id from t where id in (select id from t);",
        "select (select max(id) from t) as m from t;",
    ] {
        let select = parse_select(sql);
        assert!(build_select(&mut db, &select).unwrap().is_some(), "{sql}");
    }
}

#[test]
fn unions_build_an_operator_plan() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int);").unwrap();

    for sql in [
        "select id from t union select id from t;",
        "select id from t union all select id from t;",
        "select id from t union select id from t order by id limit 1;",
    ] {
        let select = parse_select(sql);
        assert!(build_select(&mut db, &select).unwrap().is_some(), "{sql}");
    }
}
