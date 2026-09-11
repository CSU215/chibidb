use chibidb::config::Config;
use chibidb::instance::Instance;
use chibidb::value::Value;
use chibidb::ResultSet;

fn open(dir: &tempfile::TempDir) -> Instance {
    Instance::open(dir.path(), &Config::default()).unwrap()
}

#[test]
fn starts_with_no_databases() {
    let dir = tempfile::tempdir().unwrap();
    let inst = open(&dir);
    assert!(inst.databases().unwrap().is_empty());
}

#[test]
fn database_listing_comes_from_the_registry_not_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let inst = open(&dir);
    // a stray directory is not a database; only registered ones are listed
    std::fs::create_dir_all(dir.path().join("manual")).unwrap();
    assert!(inst.databases().unwrap().is_empty());

    inst.create_database("shop").unwrap();
    assert_eq!(inst.databases().unwrap(), ["shop"]);
    // the system metadata directory is never a user database
    assert!(dir.path().join("chibi_meta").is_dir());
    assert!(!inst.databases().unwrap().iter().any(|d| d == "chibi_meta"));
}

#[test]
fn creates_and_lists_databases_sorted() {
    let dir = tempfile::tempdir().unwrap();
    let inst = open(&dir);
    inst.create_database("shop").unwrap();
    inst.create_database("blog").unwrap();
    assert_eq!(inst.databases().unwrap(), ["blog", "shop"]);
}

#[test]
fn rejects_duplicate_invalid_and_reserved_names() {
    let dir = tempfile::tempdir().unwrap();
    let inst = open(&dir);
    inst.create_database("shop").unwrap();
    assert!(inst.create_database("shop").is_err());
    assert!(inst.create_database("").is_err());
    assert!(inst.create_database("bad name").is_err());
    assert!(inst.create_database("chibi_meta").is_err());
}

#[test]
fn unknown_database_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let inst = open(&dir);
    assert!(inst.database("nope").is_err());
}

#[test]
fn databases_are_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let inst = open(&dir);
    inst.create_database("a").unwrap();
    inst.create_database("b").unwrap();

    inst.with_database_mut("a", |db| db.execute_sql("create table t (id int);").unwrap())
        .unwrap();
    inst.with_database_mut("a", |db| db.execute_sql("insert into t values (1);").unwrap())
        .unwrap();

    let missing = inst.with_database_mut("b", |db| db.execute_sql("select * from t;"));
    assert!(missing.unwrap().is_err());
}

#[test]
fn databases_persist_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let inst = open(&dir);
        inst.create_database("shop").unwrap();
        inst.with_database_mut("shop", |db| {
            db.execute_sql("create table t (id int);").unwrap();
            db.execute_sql("insert into t values (7);").unwrap();
        })
        .unwrap();
    }

    let inst = open(&dir);
    assert_eq!(inst.databases().unwrap(), ["shop"]);
    let rs = inst
        .with_database_mut("shop", |db| db.execute_sql("select id from t;").unwrap())
        .unwrap();
    match &rs[0] {
        ResultSet::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(7)),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn drops_database_and_its_files() {
    let dir = tempfile::tempdir().unwrap();
    let inst = open(&dir);
    inst.create_database("shop").unwrap();
    inst.with_database_mut("shop", |db| db.execute_sql("create table t (id int);").unwrap())
        .unwrap();

    inst.drop_database("shop").unwrap();
    assert!(inst.databases().unwrap().is_empty());
    assert!(inst.database("shop").is_err());
    assert!(!dir.path().join("shop").exists());
}
