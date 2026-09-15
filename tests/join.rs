use chaoticdb::value::Value;
use chaoticdb::Database;

fn with_dbs(f: impl Fn(&Database)) {
    let mut mem = Database::open_in_memory().unwrap();
    f(&mut mem);

    let dir = tempfile::tempdir().unwrap();
    let mut file_db = Database::open(dir.path()).unwrap();
    f(&mut file_db);
}

fn seeded(db: &Database) {
    db.execute_sql("create table dept (id int, dname char(8));").unwrap();
    db.execute_sql("insert into dept values (1, 'dev'), (2, 'ops'), (3, 'hr');")
        .unwrap();
    db.execute_sql("create table emp (id int, dept_id int, name char(8));").unwrap();
    db.execute_sql(
        "insert into emp values (10, 1, 'alice'), (11, 2, 'bob'), (12, 1, 'carol'), (13, 9, 'dan');",
    )
    .unwrap();
}

fn rows_of(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    let rs = db.execute_sql(sql).unwrap();
    match &rs[0] {
        chaoticdb::ResultSet::Rows { rows, .. } => rows.clone(),
        other => panic!("expected rows for {sql}, got {other:?}"),
    }
}

fn message_of(db: &Database, sql: &str) -> String {
    let rs = db.execute_sql(sql).unwrap();
    match &rs[0] {
        chaoticdb::ResultSet::Message(m) => m.clone(),
        other => panic!("expected message for {sql}, got {other:?}"),
    }
}

/// A comma join whose equality lives in WHERE must plan as a hash join, and
/// produce the same rows as the explicit `JOIN ... ON` form.
#[test]
fn comma_join_matches_explicit_and_hashes() {
    with_dbs(|db| {
        seeded(db);
        let comma = rows_of(
            db,
            "select emp.name, dept.dname from emp, dept where emp.dept_id = dept.id \
             order by emp.name;",
        );
        let explicit = rows_of(
            db,
            "select emp.name, dept.dname from emp join dept on emp.dept_id = dept.id \
             order by emp.name;",
        );
        assert_eq!(comma, explicit);

        let plan = message_of(
            db,
            "explain select emp.name from emp, dept where emp.dept_id = dept.id;",
        );
        assert!(plan.contains("HashJoin"), "{plan}");

        let plan =
            message_of(db, "explain select emp.name from emp join dept on emp.dept_id = dept.id;");
        assert!(plan.contains("HashJoin"), "{plan}");

        // no equi predicate between the tables stays a cross product
        let plan = message_of(db, "explain select emp.name from emp, dept;");
        assert!(plan.contains("NestedLoopJoin"), "{plan}");
    });
}

#[test]
fn comma_join_with_where() {
    with_dbs(|db| {
        seeded(db);
        let rows = rows_of(
            db,
            "select emp.name, dept.dname from emp, dept where emp.dept_id = dept.id;",
        );
        assert_eq!(rows.len(), 3, "dan has no matching dept");
        assert!(rows.contains(&vec![Value::Str("alice".into()), Value::Str("dev".into())]));
        assert!(rows.contains(&vec![Value::Str("bob".into()), Value::Str("ops".into())]));
        assert!(rows.contains(&vec![Value::Str("carol".into()), Value::Str("dev".into())]));
    });
}

#[test]
fn inner_join_on() {
    with_dbs(|db| {
        seeded(db);
        let rows = rows_of(
            db,
            "select emp.name, dept.dname from emp join dept on emp.dept_id = dept.id where dept.dname = 'dev';",
        );
        assert_eq!(rows.len(), 2);
        assert!(rows.contains(&vec![Value::Str("alice".into()), Value::Str("dev".into())]));
        assert!(rows.contains(&vec![Value::Str("carol".into()), Value::Str("dev".into())]));
    });
}

/// A single-source WHERE conjunct is pushed onto that source before the join;
/// an outer join must not be (its null-extension would change).
#[test]
fn where_is_pushed_only_through_inner_joins() {
    with_dbs(|db| {
        seeded(db);
        // both predicates belong to one side (e.id to emp, d.dname to dept)
        let inner = rows_of(
            db,
            "select e.name from emp e join dept d on e.dept_id = d.id \
             where d.dname = 'dev' and e.id > 10 order by e.name;",
        );
        assert_eq!(inner, [[Value::Str("carol".into())]]);

        // the same predicate over a LEFT join must stay above the join: pushing
        // it onto dept would still null-extend every emp, so bob/dan would leak
        let left = rows_of(
            db,
            "select e.name from emp e left join dept d on e.dept_id = d.id \
             where d.dname = 'dev' order by e.name;",
        );
        assert_eq!(left, [[Value::Str("alice".into())], [Value::Str("carol".into())]]);
    });
}

#[test]
fn join_with_aliases() {
    with_dbs(|db| {
        seeded(db);
        let rows = rows_of(
            db,
            "select e.name, d.dname from emp e join dept d on e.dept_id = d.id where d.dname = 'ops';",
        );
        assert_eq!(rows, [[Value::Str("bob".into()), Value::Str("ops".into())]]);
    });
}

#[test]
fn join_star_covers_both_tables() {
    with_dbs(|db| {
        seeded(db);
        let rs = db
            .execute_sql("select * from emp join dept on emp.dept_id = dept.id where emp.id = 10;")
            .unwrap();
        match &rs[0] {
            chaoticdb::ResultSet::Rows { columns, rows } => {
                assert_eq!(rows[0].len(), 5, "emp(3 cols) + dept(2 cols)");
                assert_eq!(columns.len(), 5);
                assert_eq!(
                    rows[0],
                    [
                        Value::Int(10),
                        Value::Int(1),
                        Value::Str("alice".into()),
                        Value::Int(1),
                        Value::Str("dev".into())
                    ]
                );
            }
            other => panic!("expected rows, got {other:?}"),
        }
    });
}

#[test]
fn three_way_join() {
    with_dbs(|db| {
        seeded(db);
        db.execute_sql("create table bonus (emp_id int, amount int);").unwrap();
        db.execute_sql("insert into bonus values (10, 100), (11, 200);").unwrap();
        let rows = rows_of(
            db,
            "select emp.name, bonus.amount from emp, dept, bonus where emp.dept_id = dept.id and bonus.emp_id = emp.id;",
        );
        assert_eq!(rows.len(), 2);
        assert!(rows.contains(&vec![Value::Str("alice".into()), Value::Int(100)]));
        assert!(rows.contains(&vec![Value::Str("bob".into()), Value::Int(200)]));
    });
}

#[test]
fn right_join_keeps_unmatched_right_rows() {
    with_dbs(|db| {
        seeded(db);
        // dept 3 (hr) has no employee, so it survives with NULL emp columns;
        // emp 13 (dan, dept_id 9) has no dept and is dropped
        let rows = rows_of(
            db,
            "select emp.name, dept.dname from emp right join dept on emp.dept_id = dept.id \
             order by dept.id;",
        );
        assert_eq!(
            rows,
            [
                [Value::Str("alice".into()), Value::Str("dev".into())],
                [Value::Str("carol".into()), Value::Str("dev".into())],
                [Value::Str("bob".into()), Value::Str("ops".into())],
                [Value::Null, Value::Str("hr".into())],
            ]
        );

        // RIGHT OUTER JOIN is accepted too
        let rows = rows_of(
            db,
            "select dept.dname from emp right outer join dept on emp.dept_id = dept.id \
             where emp.id is null;",
        );
        assert_eq!(rows, [[Value::Str("hr".into())]]);
    });
}

#[test]
fn join_errors() {
    with_dbs(|db| {
        seeded(db);
        // ambiguous unqualified column
        let err = db
            .execute_sql("select id from emp, dept where emp.dept_id = dept.id;")
            .unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");

        // unknown qualifier
        let err = db
            .execute_sql("select x.name from emp, dept;")
            .unwrap_err();
        assert!(err.to_string().contains("no such column"), "{err}");

        // join without on
        assert!(db.execute_sql("select * from emp join dept;").is_err());
    });
}

#[test]
fn join_with_group_order_limit() {
    with_dbs(|db| {
        seeded(db);
        let rs = db
            .execute_sql(
                "select d.dname, count(*) as total from emp e join dept d on e.dept_id = d.id group by d.dname order by total desc, d.dname limit 1;",
            )
            .unwrap();
        match &rs[0] {
            chaoticdb::ResultSet::Rows { rows, .. } => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0][0], Value::Str("dev".into()));
                assert_eq!(rows[0][1], Value::Int(2));
            }
            other => panic!("expected rows, got {other:?}"),
        }
    });
}

#[test]
fn comma_then_explicit_join_keeps_the_on_clause() {
    with_dbs(|db| {
        db.execute_sql("create table a (x int);").unwrap();
        db.execute_sql("create table b (k int);").unwrap();
        db.execute_sql("create table c (k int);").unwrap();
        db.execute_sql("insert into a values (1);").unwrap();
        db.execute_sql("insert into b values (1);").unwrap();
        db.execute_sql("insert into c values (1);").unwrap();

        // The ON belongs to `b join c`; a comma before it must not shift the
        // ON onto the comma join (which used to reference `c` out of scope).
        let rows = rows_of(db, "select a.x, b.k, c.k from a, b join c on b.k = c.k;");
        assert_eq!(rows, vec![vec![Value::Int(1), Value::Int(1), Value::Int(1)]]);
    });
}
