use chibidb::value::Value;
use chibidb::{Database, ResultSet};

fn rows(rs: &[ResultSet]) -> (&[String], &[Vec<Value>]) {
    match &rs[0] {
        ResultSet::Rows { columns, rows } => (columns, rows),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn with_dbs(f: impl Fn(&Database)) {
    let mut mem = Database::open_in_memory().unwrap();
    f(&mut mem);

    let dir = tempfile::tempdir().unwrap();
    let mut file_db = Database::open(dir.path()).unwrap();
    f(&mut file_db);
}

fn message(rs: &[ResultSet]) -> String {
    match &rs[0] {
        ResultSet::Message(m) => m.clone(),
        other => panic!("expected message, got {other:?}"),
    }
}

fn rows_of(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    let rs = db.execute_sql(sql).unwrap();
    match &rs[0] {
        ResultSet::Rows { rows, .. } => rows.clone(),
        other => panic!("expected rows for {sql}, got {other:?}"),
    }
}

fn seeded(db: &Database) {
    db.execute_sql("create table student (id int, name char(10), score float);")
        .unwrap();
    for i in 0..200i64 {
        db.execute_sql(&format!(
            "insert into student values ({i}, 'name{i:03}', {}.5);",
            i % 100
        ))
        .unwrap();
    }
}

#[test]
fn index_ddl_validation() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int, name char(10));").unwrap();

        let m = message(&db.execute_sql("create index idx on t (id);").unwrap());
        assert_eq!(m, "SUCCESS");

        let err = db.execute_sql("create index idx on t (id);").unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");

        let err = db.execute_sql("create index i2 on missing (id);").unwrap_err();
        assert!(err.to_string().contains("no such table"), "{err}");

        let err = db.execute_sql("create index i3 on t (nope);").unwrap_err();
        assert!(err.to_string().contains("no such column"), "{err}");

        let m = message(&db.execute_sql("drop index idx;").unwrap());
        assert_eq!(m, "SUCCESS");

        let err = db.execute_sql("drop index idx;").unwrap_err();
        assert!(err.to_string().contains("no such index"), "{err}");

        // recreating after drop works
        db.execute_sql("create index idx on t (id);").unwrap();
    });
}

#[test]
fn explain_chooses_access_path() {
    with_dbs(|db| {
        seeded(db);

        let plan = message(&db.execute_sql("explain select * from student where id = 5;").unwrap());
        assert!(plan.contains("FullScan"), "{plan}");

        db.execute_sql("create index idx_id on student (id);").unwrap();

        let plan = message(&db.execute_sql("explain select * from student where id = 5;").unwrap());
        assert!(plan.contains("IndexScan"), "{plan}");
        assert!(plan.contains("idx_id"), "{plan}");

        // range predicates also use the index
        let plan = message(
            &db.execute_sql("explain select * from student where id >= 10 and id < 20;")
                .unwrap(),
        );
        assert!(plan.contains("IndexScan"), "{plan}");

        // non-indexed column stays full scan
        let plan =
            message(&db.execute_sql("explain select * from student where name = 'x';").unwrap());
        assert!(plan.contains("FullScan"), "{plan}");
    });
}

#[test]
fn ordered_index_scan_handles_stale_versions() {
    with_dbs(|db| {
        db.execute_sql("create table t (id int, v int);").unwrap();
        db.execute_sql("insert into t values (1,10),(2,20),(3,30),(4,40),(5,50);").unwrap();
        db.execute_sql("create index idx on t (id);").unwrap();
        // an update leaves the old version's index entry behind for snapshots
        db.execute_sql("update t set v = 99 where id = 2;").unwrap();
        db.execute_sql("delete from t where id = 4;").unwrap();
        db.execute_sql("insert into t values (3, 33);").unwrap();

        let ids = rows_of(db, "select id from t order by id;");
        assert_eq!(
            ids,
            vec![
                vec![Value::Int(1)],
                vec![Value::Int(2)],
                vec![Value::Int(3)],
                vec![Value::Int(3)],
                vec![Value::Int(5)],
            ]
        );
        let top2 = rows_of(db, "select id from t order by id limit 2;");
        assert_eq!(top2, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    });
}

#[test]
fn explain_uses_index_for_order_by() {
    with_dbs(|db| {
        seeded(db);
        let sql = "select id from student order by id limit 10;";

        let plan = message(&db.execute_sql(&format!("explain {sql}")).unwrap());
        assert!(plan.contains("FullScan"), "{plan}");
        let before = rows_of(db, sql);

        db.execute_sql("create index idx_id on student (id);").unwrap();
        let plan = message(&db.execute_sql(&format!("explain {sql}")).unwrap());
        assert!(plan.contains("OrderedIndexScan"), "{plan}");
        assert_eq!(before, rows_of(db, sql));

        // descending order still needs an explicit sort
        let plan = message(
            &db.execute_sql("explain select id from student order by id desc limit 10;")
                .unwrap(),
        );
        assert!(!plan.contains("OrderedIndexScan"), "{plan}");
    });
}

#[test]
fn explain_combines_range_bounds() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int);").unwrap();
    db.execute_sql("insert into t values (1),(2),(3),(4),(5);").unwrap();
    db.execute_sql("create index idx on t (id);").unwrap();

    let plan =
        message(&db.execute_sql("explain select * from t where id >= 2 and id < 5;").unwrap());
    assert!(plan.contains("id >= 2"), "lower bound missing: {plan}");
    assert!(plan.contains("id < 5"), "upper bound missing: {plan}");

    let rs = db
        .execute_sql("select id from t where id >= 2 and id < 5 order by id;")
        .unwrap();
    let (_, rows) = rows(&rs);
    assert_eq!(rows, [[Value::Int(2)], [Value::Int(3)], [Value::Int(4)]]);
}

#[test]
fn index_order_by_skips_sort() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int, name char(10));").unwrap();
    for i in 0..50i64 {
        db.execute_sql(&format!("insert into t values ({i}, 'n{i:02}');")).unwrap();
    }
    db.execute_sql("create index idx on t (id);").unwrap();

    let plan =
        message(&db.execute_sql("explain select * from t where id > 5 order by id;").unwrap());
    assert!(plan.contains("OrderedIndexScan"), "ascending order uses the index: {plan}");

    let plan = message(
        &db.execute_sql("explain select * from t where id > 5 order by id desc;").unwrap(),
    );
    assert!(!plan.contains("OrderedIndexScan"), "descending still sorts: {plan}");

    let plan =
        message(&db.execute_sql("explain select * from t where id > 5 order by name;").unwrap());
    assert!(!plan.contains("OrderedIndexScan"), "other column still sorts: {plan}");

    // and the rows really come out ascending without an explicit sort step
    let rs = db.execute_sql("select id from t where id > 45 order by id;").unwrap();
    let (_, rows) = rows(&rs);
    assert_eq!(
        rows,
        [[Value::Int(46)], [Value::Int(47)], [Value::Int(48)], [Value::Int(49)]]
    );
}

#[test]
fn index_scan_results_match_full_scan() {
    with_dbs(|db| {
        seeded(db);
        db.execute_sql("create index idx_id on student (id);").unwrap();

        for sql in [
            "select name from student where id = 42;",
            "select name from student where id < 5;",
            "select name from student where id >= 195;",
            "select id from student where id > 10 and id <= 13;",
        ] {
            let rs = db.execute_sql(sql).unwrap();
            let (_, got) = rows(&rs);
            assert!(!got.is_empty(), "{sql}");
            for row in got {
                let id = match &row[0] {
                    Value::Str(s) => s.trim_start_matches("name").parse::<i64>().unwrap(),
                    Value::Int(n) => *n,
                    other => panic!("{sql}: unexpected {other:?}"),
                };
                let ok = sql.contains("id < 5") && id < 5
                    || sql.contains("id >= 195") && id >= 195
                    || sql.contains("id > 10") && (11..=13).contains(&id)
                    || id == 42;
                assert!(ok, "{sql} returned inconsistent row {id}");
            }
        }
    });
}

#[test]
fn index_maintenance_through_dml() {
    with_dbs(|db| {
        seeded(db);
        db.execute_sql("create index idx_id on student (id);").unwrap();

        db.execute_sql("delete from student where id < 10;").unwrap();
        db.execute_sql("update student set id = 1000 where id = 100;").unwrap();
        db.execute_sql("insert into student values (7, 'reborn', 1.5);").unwrap();

        let rs = db.execute_sql("select name from student where id = 7;").unwrap();
        let (_, got) = rows(&rs);
        assert_eq!(got, [[Value::Str("reborn".into())]]);

        let rs = db.execute_sql("select name from student where id = 100;").unwrap();
        let (_, got) = rows(&rs);
        assert_eq!(got.len(), 0, "old key must be gone");

        let rs = db.execute_sql("select name from student where id = 1000;").unwrap();
        let (_, got) = rows(&rs);
        assert_eq!(got, [[Value::Str("name100".into())]]);
    });
}

#[test]
fn index_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        seeded(&db);
        db.execute_sql("create index idx_id on student (id);").unwrap();
    }
    let db = Database::open(dir.path()).unwrap();

    let plan = message(&db.execute_sql("explain select * from student where id = 5;").unwrap());
    assert!(plan.contains("IndexScan"), "{plan}");

    let rs = db.execute_sql("select name from student where id = 123;").unwrap();
    let (_, got) = rows(&rs);
    assert_eq!(got, [[Value::Str("name123".into())]]);

    // index still maintained after reopen
    db.execute_sql("insert into student values (500, 'new', 0.5);").unwrap();
    let rs = db.execute_sql("select name from student where id = 500;").unwrap();
    let (_, got) = rows(&rs);
    assert_eq!(got, [[Value::Str("new".into())]]);
}
