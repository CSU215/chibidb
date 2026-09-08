use chibidb::Database;

#[test]
fn file_backend_writes_data_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path()).unwrap();
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
    let mut db = Database::open(dir.path()).unwrap();
    db.execute_sql("create table a (id int);").unwrap();
    db.execute_sql("create table b (id int);").unwrap();
    db.execute_sql("create table C (id int);").unwrap();

    let tables_dir = dir.path().join("tables");
    let count = std::fs::read_dir(&tables_dir).unwrap().count();
    assert_eq!(count, 3, "windows-style case-insensitive names must not collide");
}
