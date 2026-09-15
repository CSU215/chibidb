//! EXPLAIN now prints a LogicalOperator tree followed by the physical tree.

use chaoticdb::value::Value;
use chaoticdb::{Database, ResultSet};

fn message(db: &Database, sql: &str) -> String {
    match &db.execute_sql(sql).unwrap()[0] {
        ResultSet::Message(m) => m.clone(),
        other => panic!("expected message, got {other:?}"),
    }
}

fn seeded() -> Database {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table dept (id int, dname char(8));").unwrap();
    db.execute_sql("insert into dept values (1, 'dev'), (2, 'ops');").unwrap();
    db.execute_sql("create table emp (id int, dept_id int, name char(8));").unwrap();
    db.execute_sql("insert into emp values (10, 1, 'alice'), (11, 2, 'bob');").unwrap();
    db
}

#[test]
fn explain_prints_both_plans() {
    let db = seeded();
    let plan = message(&db, "explain select name from emp where id = 10;");
    assert!(plan.starts_with("LogicalPlan:"), "{plan}");
    assert!(plan.contains("PhysicalPlan:"), "{plan}");
    // the logical tree is a logical operator, not a physical access path
    assert!(plan.contains("Scan emp"), "{plan}");
    assert!(plan.contains("Filter"), "{plan}");
    assert!(plan.contains("Project"), "{plan}");
    // the physical tree still reports the access path
    assert!(plan.contains("FullScan") || plan.contains("IndexScan"), "{plan}");
}

#[test]
fn explain_logical_join_and_aggregate() {
    let db = seeded();

    let plan = message(
        &db,
        "explain select emp.name, dept.dname from emp join dept on emp.dept_id = dept.id;",
    );
    assert!(plan.contains("Join Inner"), "{plan}");
    assert!(plan.contains("HashJoin"), "{plan}");
    assert!(!plan.contains("LogicalJoin"), "{plan}");

    let plan = message(&db, "explain select dept_id, count(*) from emp group by dept_id;");
    assert!(plan.contains("Aggregate"), "{plan}");
    assert!(plan.contains("Project"), "{plan}");
    assert!(plan.contains("Scan emp"), "{plan}");

    // the aggregate tail is split into standard logical nodes
    let plan = message(
        &db,
        "explain select dept_id, count(*) from emp group by dept_id \
         having count(*) > 0 order by dept_id;",
    );
    assert!(plan.contains("Having"), "{plan}");
    assert!(plan.contains("Sort"), "{plan}");
    assert!(plan.contains("Aggregate"), "{plan}");
}

#[test]
fn explain_matches_query_results() {
    let db = seeded();
    let rs = db.execute_sql("select dept_id, count(*) from emp group by dept_id order by dept_id;")
        .unwrap();
    match &rs[0] {
        ResultSet::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0], vec![Value::Int(1), Value::Int(1)]);
            assert_eq!(rows[1], vec![Value::Int(2), Value::Int(1)]);
        }
        other => panic!("expected rows, got {other:?}"),
    }
}
