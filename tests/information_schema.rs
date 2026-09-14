use chaoticdb::config::Config;
use chaoticdb::instance::Instance;
use chaoticdb::value::Value;
use chaoticdb::{ResultSet, Session};

fn rows(inst: &Instance, session: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match inst.execute_with(session, sql).unwrap().remove(0) {
        ResultSet::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn information_schema_exposes_databases_tables_and_columns() {
    let dir = tempfile::tempdir().unwrap();
    let inst = Instance::open(dir.path(), &Config::default()).unwrap();
    let mut s = Session::new();
    inst.execute_with(&mut s, "create database shop;").unwrap();
    inst.execute_with(&mut s, "use shop;").unwrap();
    inst.execute_with(&mut s, "create table t (id int primary key, name char(8));").unwrap();

    inst.execute_with(&mut s, "use information_schema;").unwrap();

    let schemas = rows(&inst, &mut s, "select schema_name from schemata order by schema_name;");
    assert!(schemas.iter().any(|r| r[0] == Value::Str("shop".into())));

    let tables = rows(
        &inst,
        &mut s,
        "select table_schema, table_name, engine from tables where table_name = 't';",
    );
    assert_eq!(
        tables,
        [[Value::Str("shop".into()), Value::Str("t".into()), Value::Str("heap".into())]]
    );

    let columns = rows(
        &inst,
        &mut s,
        "select column_name from columns where table_name = 't' order by column_name;",
    );
    assert_eq!(columns, [[Value::Str("id".into())], [Value::Str("name".into())]]);
}

#[test]
fn information_schema_reports_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let inst = Instance::open(dir.path(), &Config::default()).unwrap();
    let mut s = Session::new();
    inst.execute_with(&mut s, "create database shop;").unwrap();
    inst.execute_with(&mut s, "use shop;").unwrap();
    inst.execute_with(&mut s, "create table l (id int) engine = lsm;").unwrap();

    inst.execute_with(&mut s, "use information_schema;").unwrap();
    let engine = rows(&inst, &mut s, "select engine from tables where table_name = 'l';");
    assert_eq!(engine, [[Value::Str("lsm".into())]]);
}

#[test]
fn information_schema_reports_the_page_layout() {
    let dir = tempfile::tempdir().unwrap();
    {
        let inst = Instance::open(dir.path(), &Config::default()).unwrap();
        let mut s = Session::new();
        inst.execute_with(&mut s, "create database shop;").unwrap();
        inst.execute_with(&mut s, "use shop;").unwrap();
        inst.execute_with(&mut s, "create table w (id int, v int) page_layout = pax;")
            .unwrap();
    }
    // the layout is recorded in the catalog and survives a reopen
    let inst = Instance::open(dir.path(), &Config::default()).unwrap();
    let mut s = Session::new();
    inst.execute_with(&mut s, "use information_schema;").unwrap();
    let layout = rows(&inst, &mut s, "select page_layout from tables where table_name = 'w';");
    assert_eq!(layout, [[Value::Str("pax".into())]]);
}

#[test]
fn information_schema_is_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let inst = Instance::open(dir.path(), &Config::default()).unwrap();
    let mut s = Session::new();
    inst.execute_with(&mut s, "use information_schema;").unwrap();
    assert!(inst.execute_with(&mut s, "create table x (id int);").is_err());
}
