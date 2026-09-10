//! End-to-end scenarios modeled on the classic miniob demo schema
//! (student / course / sc): CRUD, aggregation, joins, subqueries, and
//! index access paths combined the way miniob exercises them.

use chibidb::value::Value;
use chibidb::{Database, ResultSet};

fn query(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    let rs = db.execute_sql(sql).unwrap();
    assert_eq!(rs.len(), 1, "sql: {sql}");
    match &rs[0] {
        ResultSet::Rows { rows, .. } => rows.clone(),
        other => panic!("expected rows for `{sql}`, got {other:?}"),
    }
}

fn message(db: &mut Database, sql: &str) -> String {
    let rs = db.execute_sql(sql).unwrap();
    assert_eq!(rs.len(), 1, "sql: {sql}");
    match &rs[0] {
        ResultSet::Message(m) => m.clone(),
        other => panic!("expected message for `{sql}`, got {other:?}"),
    }
}

fn seed(db: &mut Database) {
    db.execute_sql("create table student (sno int, sname char(20), sage int, ssex char(2));")
        .unwrap();
    db.execute_sql(
        "insert into student values \
         (1,'alice',20,'F'),(2,'bob',21,'M'),(3,'carol',22,'F'),(4,'dave',23,'M'),(5,'eve',24,'F');",
    )
    .unwrap();
    db.execute_sql("create table course (cno int, cname char(20));").unwrap();
    db.execute_sql("insert into course values (101,'math'),(102,'physics'),(103,'chemistry');")
        .unwrap();
    db.execute_sql("create table sc (sno int, cno int, grade int);").unwrap();
    db.execute_sql(
        "insert into sc values (1,101,90),(1,102,85),(2,101,70),(3,103,95),(4,102,60);",
    )
    .unwrap();
}

fn with_db(f: impl Fn(&mut Database)) {
    let mut db = Database::open_in_memory().unwrap();
    seed(&mut db);
    f(&mut db);
}

#[test]
fn crud_then_aggregates() {
    with_db(|db| {
        assert_eq!(query(db, "select count(*) from student;"), [[Value::Int(5)]]);
        assert_eq!(
            query(db, "select sum(sage), avg(sage), min(sage), max(sage) from student;"),
            [[Value::Int(110), Value::Float(22.0), Value::Int(20), Value::Int(24)]]
        );

        message(db, "update student set sage = 25 where sno = 5;");
        assert_eq!(query(db, "select sage from student where sno = 5;"), [[Value::Int(25)]]);

        message(db, "delete from student where ssex = 'M';");
        assert_eq!(query(db, "select count(*) from student;"), [[Value::Int(3)]]);
        assert_eq!(
            query(db, "select sno from student order by sno;"),
            [[Value::Int(1)], [Value::Int(3)], [Value::Int(5)]]
        );
    });
}

#[test]
fn group_by_and_having() {
    with_db(|db| {
        assert_eq!(
            query(db, "select ssex, count(*) from student group by ssex;"),
            [[Value::Str("F".into()), Value::Int(3)], [Value::Str("M".into()), Value::Int(2)]]
        );
        assert_eq!(
            query(db, "select ssex, count(*) from student group by ssex having count(*) > 2;"),
            [[Value::Str("F".into()), Value::Int(3)]]
        );
        assert_eq!(
            query(db, "select cno, avg(grade) from sc group by cno order by cno;"),
            [
                [Value::Int(101), Value::Float(80.0)],
                [Value::Int(102), Value::Float(72.5)],
                [Value::Int(103), Value::Float(95.0)],
            ]
        );
        assert_eq!(
            query(
                db,
                "select cno, count(*) from sc group by cno having avg(grade) >= 80 order by cno;"
            ),
            [[Value::Int(101), Value::Int(2)], [Value::Int(103), Value::Int(1)]]
        );
    });
}

#[test]
fn inner_and_left_joins() {
    with_db(|db| {
        assert_eq!(
            query(
                db,
                "select sname, cname, grade from student \
                 join sc on student.sno = sc.sno \
                 join course on sc.cno = course.cno \
                 order by grade desc;"
            ),
            [
                [Value::Str("carol".into()), Value::Str("chemistry".into()), Value::Int(95)],
                [Value::Str("alice".into()), Value::Str("math".into()), Value::Int(90)],
                [Value::Str("alice".into()), Value::Str("physics".into()), Value::Int(85)],
                [Value::Str("bob".into()), Value::Str("math".into()), Value::Int(70)],
                [Value::Str("dave".into()), Value::Str("physics".into()), Value::Int(60)],
            ]
        );
        assert_eq!(
            query(
                db,
                "select sname, grade from student \
                 left join sc on student.sno = sc.sno order by sname;"
            ),
            [
                [Value::Str("alice".into()), Value::Int(90)],
                [Value::Str("alice".into()), Value::Int(85)],
                [Value::Str("bob".into()), Value::Int(70)],
                [Value::Str("carol".into()), Value::Int(95)],
                [Value::Str("dave".into()), Value::Int(60)],
                [Value::Str("eve".into()), Value::Null],
            ]
        );
    });
}

#[test]
fn uncorrelated_subqueries() {
    with_db(|db| {
        assert_eq!(
            query(
                db,
                "select sname from student \
                 where sno in (select sno from sc where grade >= 90) order by sname;"
            ),
            [[Value::Str("alice".into())], [Value::Str("carol".into())]]
        );
        assert_eq!(
            query(
                db,
                "select sname from student \
                 where sage > (select avg(sage) from student) order by sname;"
            ),
            [[Value::Str("dave".into())], [Value::Str("eve".into())]]
        );
        assert_eq!(
            query(
                db,
                "select cname from course \
                 where cno in (select cno from sc where grade < 75) order by cname;"
            ),
            [[Value::Str("math".into())], [Value::Str("physics".into())]]
        );
        assert_eq!(
            query(db, "select cname from course where cno not in (select cno from sc);"),
            Vec::<Vec<Value>>::new()
        );
    });
}

#[test]
fn index_scan_matches_full_scan() {
    with_db(|db| {
        message(db, "create index idx_sno on student (sno);");
        assert!(message(db, "explain select * from student where sno = 3;")
            .contains("IndexScan(index=idx_sno, table=student"));

        assert_eq!(query(db, "select sname from student where sno = 3;"), [[Value::Str("carol".into())]]);
        assert_eq!(
            query(db, "select sname from student where sno >= 3 order by sno;"),
            [
                [Value::Str("carol".into())],
                [Value::Str("dave".into())],
                [Value::Str("eve".into())],
            ]
        );
        // a non-indexed predicate still returns the correct rows
        assert_eq!(
            query(db, "select sname from student where ssex = 'M' order by sno;"),
            [[Value::Str("bob".into())], [Value::Str("dave".into())]]
        );
    });
}
