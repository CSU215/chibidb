use chibidb::ast::{BinOp, Expr, Stmt};
use chibidb::exec::operator::{build_select, build_statement, Filter, Limit, Project, TableScan};
use chibidb::value::Value;
use chibidb::{Database, Session};

fn seed(db: &Database) {
    db.execute_sql("create table t (id int);").unwrap();
    db.execute_sql("insert into t values (1),(2),(3),(4);").unwrap();
}

#[test]
fn scan_filter_project_and_limit() {
    let db = Database::open_in_memory().unwrap();
    seed(&db);
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
    let db = Database::open_in_memory().unwrap();
    seed(&db);
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
    let db = Database::open_in_memory().unwrap();
    seed(&db);
    let mut session = Session::new();

    let scan = TableScan::new(&db, "t").unwrap();
    let mut plan = Limit::new(Box::new(scan), 1, Some(2));
    let rows = db.collect_plan(&mut session, &mut plan).unwrap();
    assert_eq!(rows, vec![vec![Value::Int(2)], vec![Value::Int(3)]]);
}

#[test]
fn project_evaluates_expressions() {
    let db = Database::open_in_memory().unwrap();
    seed(&db);
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
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int, tag int);").unwrap();
    db.execute_sql("create index idx on t (id);").unwrap();

    let select = parse_select("select id from t where id = 1 limit 1;");
    assert!(build_select(&db, &select).unwrap().is_some());
}

#[test]
fn constant_and_distinct_selects_build_plans() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int);").unwrap();

    for sql in [
        "select 1 + 2;",
        "select distinct id from t;",
        "select id from t order by id;",
        "select count(*) from t;",
        "select id, count(*) from t group by id;",
    ] {
        let select = parse_select(sql);
        assert!(build_select(&db, &select).unwrap().is_some(), "{sql}");
    }
}

#[test]
fn joins_build_an_operator_plan() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table a (id int);").unwrap();
    db.execute_sql("create table b (id int);").unwrap();

    for sql in [
        "select * from a, b;",
        "select * from a join b on a.id = b.id;",
        "select * from a left join b on a.id = b.id;",
        "select * from a right join b on a.id = b.id;",
    ] {
        let select = parse_select(sql);
        assert!(build_select(&db, &select).unwrap().is_some(), "{sql}");
    }
}

#[test]
fn representative_selects_all_build_plans() {
    let db = Database::open_in_memory().unwrap();
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
        assert!(build_select(&db, &select).unwrap().is_some(), "{sql}");
    }
}

#[test]
fn hash_join_handles_duplicate_keys() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table a (k int, x int);").unwrap();
    db.execute_sql("create table b (k int, y int);").unwrap();
    db.execute_sql("insert into a values (1, 10), (2, 20), (2, 21);").unwrap();
    db.execute_sql("insert into b values (2, 200), (2, 201), (3, 300);").unwrap();

    let rs = db
        .execute_sql("select a.x, b.y from a join b on a.k = b.k order by a.x, b.y;")
        .unwrap();
    match &rs[0] {
        chibidb::ResultSet::Rows { rows, .. } => assert_eq!(
            rows,
            &vec![
                vec![Value::Int(20), Value::Int(200)],
                vec![Value::Int(20), Value::Int(201)],
                vec![Value::Int(21), Value::Int(200)],
                vec![Value::Int(21), Value::Int(201)],
            ]
        ),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn hash_join_left_keeps_unmatched_rows() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table a (k int);").unwrap();
    db.execute_sql("create table b (k int, y int);").unwrap();
    db.execute_sql("insert into a values (1), (2);").unwrap();
    db.execute_sql("insert into b values (2, 200);").unwrap();

    let rs = db
        .execute_sql("select a.k, b.y from a left join b on a.k = b.k order by a.k;")
        .unwrap();
    match &rs[0] {
        chibidb::ResultSet::Rows { rows, .. } => assert_eq!(
            rows,
            &vec![
                vec![Value::Int(1), Value::Null],
                vec![Value::Int(2), Value::Int(200)],
            ]
        ),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn views_build_an_operator_plan() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int, tag int);").unwrap();
    db.execute_sql("create view v as select id from t where id > 0;").unwrap();

    for sql in [
        "select * from v;",
        "select id from v order by id;",
        "select a.id from t a, v b where a.id = b.id;",
    ] {
        let select = parse_select(sql);
        assert!(build_select(&db, &select).unwrap().is_some(), "{sql}");
    }
}

#[test]
fn dml_builds_operator_plans() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int);").unwrap();

    for sql in [
        "insert into t values (1);",
        "update t set id = 2 where id = 1;",
        "delete from t where id = 2;",
    ] {
        let stmt = chibidb::parser::parse(sql).unwrap().remove(0);
        assert!(build_statement(&db, &stmt).unwrap().is_some(), "{sql}");
    }

    // DDL is not an operator statement
    let ddl = chibidb::parser::parse("create table u (id int);").unwrap().remove(0);
    assert!(build_statement(&db, &ddl).unwrap().is_none());
}

#[test]
fn unions_build_an_operator_plan() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int);").unwrap();

    for sql in [
        "select id from t union select id from t;",
        "select id from t union all select id from t;",
        "select id from t union select id from t order by id limit 1;",
    ] {
        let select = parse_select(sql);
        assert!(build_select(&db, &select).unwrap().is_some(), "{sql}");
    }
}

#[test]
fn float_literal_on_int_index_falls_back_to_scan() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int primary key, v int);").unwrap();
    db.execute_sql("insert into t values (1, 10), (2, 20);").unwrap();

    let rows = |sql: &str| -> Vec<Vec<Value>> {
        match &db.execute_sql(sql).unwrap()[0] {
            chibidb::ResultSet::Rows { rows, .. } => rows.clone(),
            other => panic!("expected rows for {sql}, got {other:?}"),
        }
    };
    // These must not error just because the column has a primary-key index;
    // the row path compares with cmp_values.
    assert_eq!(rows("select id from t where id = 1.0;"), vec![vec![Value::Int(1)]]);
    assert_eq!(rows("select id from t where id > 1.5;"), vec![vec![Value::Int(2)]]);
}
