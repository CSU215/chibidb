use chibidb::Database;

#[test]
fn file_backend_writes_data_files() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    db.execute_sql("create table t (id int, name char(10));").unwrap();
    db.execute_sql("insert into t values (1, 'a'), (2, 'b');").unwrap();

    let tables_dir = dir.path().join("tables");
    let entries: Vec<_> = std::fs::read_dir(&tables_dir).unwrap().collect();
    assert_eq!(entries.len(), 1, "one data file expected");
    let len = std::fs::metadata(entries[0].as_ref().unwrap().path()).unwrap().len();
    assert!(len > 0, "data file must not be empty, got {len} bytes");
}

#[test]
fn file_backend_creates_one_file_per_table() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    db.execute_sql("create table a (id int);").unwrap();
    db.execute_sql("create table b (id int);").unwrap();
    db.execute_sql("create table C (id int);").unwrap();

    let tables_dir = dir.path().join("tables");
    let count = std::fs::read_dir(&tables_dir).unwrap().count();
    assert_eq!(count, 3, "windows-style case-insensitive names must not collide");
}

#[test]
fn reopen_restores_schema_and_data() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table student (id int, name char(10), score float);")
            .unwrap();
        db.execute_sql("insert into student values (1, 'alice', 95.5), (2, 'bob', 80);")
            .unwrap();
    }

    let db = Database::open(dir.path()).unwrap();
    let rs = db.execute_sql("select * from student;").unwrap();
    match &rs[0] {
        chibidb::ResultSet::Rows { columns, rows } => {
            assert_eq!(columns.as_slice(), ["id", "name", "score"]);
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][1].to_string(), "alice");
        }
        other => panic!("expected rows, got {other:?}"),
    }

    db.execute_sql("insert into student values (3, 'carol', 90);").unwrap();
    db.execute_sql("update student set score = 99 where id = 3;").unwrap();
    let rs = db.execute_sql("select id, score from student where id = 3;").unwrap();
    match &rs[0] {
        chibidb::ResultSet::Rows { rows, .. } => assert_eq!(
            rows.as_slice(),
            [[chibidb::value::Value::Int(3), chibidb::value::Value::Float(99.0)]]
        ),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn file_counter_continues_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table a (id int);").unwrap();
        db.execute_sql("create table b (id int);").unwrap();
    }
    let db = Database::open(dir.path()).unwrap();
    db.execute_sql("create table c (id int);").unwrap();
    db.execute_sql("insert into c values (42);").unwrap();

    let count = std::fs::read_dir(dir.path().join("tables")).unwrap().count();
    assert_eq!(count, 3);
    for t in ["a", "b", "c"] {
        assert!(db.execute_sql(&format!("select * from {t};")).is_ok(), "table {t}");
    }
}

#[test]
fn reopen_rejects_corrupt_catalog() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int);").unwrap();
    }
    std::fs::write(dir.path().join("catalog.bin"), b"garbage!!!").unwrap();
    assert!(Database::open(dir.path()).is_err());
}
