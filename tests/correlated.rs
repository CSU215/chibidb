//! Correlated subqueries: inner queries that reference the enclosing row.

use chibidb::value::Value;
use chibidb::{Database, ResultSet};

fn q(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    let rs = db.execute_sql(sql).unwrap();
    assert_eq!(rs.len(), 1, "sql: {sql}");
    match &rs[0] {
        ResultSet::Rows { rows, .. } => rows.clone(),
        other => panic!("expected rows for `{sql}`, got {other:?}"),
    }
}

fn setup(db: &Database) {
    db.execute_sql("create table student (sno int, sname char(20), sage int);").unwrap();
    db.execute_sql(
        "insert into student values (1,'alice',20),(2,'bob',21),(3,'carol',22),(4,'dave',23);",
    )
    .unwrap();
    db.execute_sql("create table sc (sno int, cno int, grade int);").unwrap();
    db.execute_sql("insert into sc values (1,101,90),(1,102,85),(2,101,70),(3,103,95);")
        .unwrap();
}

fn with_db(f: impl Fn(&Database)) {
    let db = Database::open_in_memory().unwrap();
    setup(&db);
    f(&db);
}

#[test]
fn correlated_exists_and_not_exists() {
    with_db(|db| {
        assert_eq!(
            q(
                db,
                "select sname from student s \
                 where exists (select 1 from sc where sc.sno = s.sno) order by sname;"
            ),
            [[Value::Str("alice".into())], [Value::Str("bob".into())], [Value::Str("carol".into())]]
        );
        assert_eq!(
            q(
                db,
                "select sname from student s \
                 where not exists (select 1 from sc where sc.sno = s.sno) order by sname;"
            ),
            [[Value::Str("dave".into())]]
        );
    });
}

#[test]
fn correlated_scalar_in_projection() {
    with_db(|db| {
        assert_eq!(
            q(
                db,
                "select sname, (select count(*) from sc where sc.sno = s.sno) as courses \
                 from student s order by sname;"
            ),
            [
                [Value::Str("alice".into()), Value::Int(2)],
                [Value::Str("bob".into()), Value::Int(1)],
                [Value::Str("carol".into()), Value::Int(1)],
                [Value::Str("dave".into()), Value::Int(0)],
            ]
        );
    });
}

#[test]
fn correlated_scalar_in_where() {
    with_db(|db| {
        assert_eq!(
            q(
                db,
                "select sname from student s \
                 where (select count(*) from sc where sc.sno = s.sno) > 1 order by sname;"
            ),
            [[Value::Str("alice".into())]]
        );
    });
}

#[test]
fn correlated_join_predicate_over_two_outer_columns() {
    with_db(|db| {
        assert_eq!(
            q(
                db,
                "select sname from student s \
                 where exists (select 1 from sc where sc.sno = s.sno and sc.grade > s.sage) \
                 order by sname;"
            ),
            [[Value::Str("alice".into())], [Value::Str("bob".into())], [Value::Str("carol".into())]]
        );
    });
}

#[test]
fn nested_correlated_subquery_reaches_grandparent() {
    with_db(|db| {
        // the innermost subquery references `s` two query levels up
        assert_eq!(
            q(
                db,
                "select sname from student s where exists ( \
                   select 1 from sc \
                   where sc.sno = s.sno \
                     and sc.grade = (select max(grade) from sc s3 where s3.sno = s.sno) \
                 ) order by sname;"
            ),
            [[Value::Str("alice".into())], [Value::Str("bob".into())], [Value::Str("carol".into())]]
        );
    });
}
