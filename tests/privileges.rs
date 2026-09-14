use chaoticdb::sql::ast::Privilege;
use chaoticdb::config::Config;
use chaoticdb::instance::Instance;
use chaoticdb::Session;

fn instance(dir: &tempfile::TempDir) -> Instance {
    Instance::open(dir.path(), &Config::default()).unwrap()
}

fn setup(inst: &Instance, s: &mut Session) {
    inst.execute_with(s, "create user alice identified by 'x';").unwrap();
    inst.execute_with(s, "create database shop;").unwrap();
}

#[test]
fn grant_and_check_read_privilege() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();
    setup(&inst, &mut s);

    assert!(!inst.has_privilege("alice", "shop", Privilege::Read).unwrap());
    inst.execute_with(&mut s, "grant read on shop to alice;").unwrap();
    assert!(inst.has_privilege("alice", "shop", Privilege::Read).unwrap());
    assert!(!inst.has_privilege("alice", "shop", Privilege::Write).unwrap());
}

#[test]
fn grant_all_on_all_databases() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();
    setup(&inst, &mut s);

    inst.execute_with(&mut s, "grant all on * to alice;").unwrap();
    assert!(inst.has_privilege("alice", "shop", Privilege::Read).unwrap());
    assert!(inst.has_privilege("alice", "other", Privilege::Write).unwrap());
    assert!(!inst.has_privilege("bob", "shop", Privilege::Read).unwrap());
}

#[test]
fn revoke_removes_the_privilege() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();
    setup(&inst, &mut s);

    inst.execute_with(&mut s, "grant write on shop to alice;").unwrap();
    assert!(inst.has_privilege("alice", "shop", Privilege::Write).unwrap());
    inst.execute_with(&mut s, "revoke write on shop from alice;").unwrap();
    assert!(!inst.has_privilege("alice", "shop", Privilege::Write).unwrap());
}

#[test]
fn granting_to_unknown_user_errors() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();
    setup(&inst, &mut s);
    assert!(inst.execute_with(&mut s, "grant read on shop to bob;").is_err());
}

#[test]
fn dropping_a_user_removes_its_privileges() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();
    setup(&inst, &mut s);
    inst.execute_with(&mut s, "grant all on * to alice;").unwrap();
    assert!(inst.has_privilege("alice", "shop", Privilege::Read).unwrap());

    inst.execute_with(&mut s, "drop user alice;").unwrap();
    // recreate the same name: old grants must not come back
    inst.execute_with(&mut s, "create user alice identified by 'x';").unwrap();
    assert!(!inst.has_privilege("alice", "shop", Privilege::Read).unwrap());
    assert!(!inst.has_privilege("alice", "shop", Privilege::Write).unwrap());
}

#[test]
fn dropping_a_database_removes_its_privileges() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();
    setup(&inst, &mut s);
    inst.execute_with(&mut s, "grant all on shop to alice;").unwrap();
    assert!(inst.has_privilege("alice", "shop", Privilege::Read).unwrap());

    inst.execute_with(&mut s, "drop database shop;").unwrap();
    inst.execute_with(&mut s, "create database shop;").unwrap();
    // recreate the same database: old grants must not come back
    assert!(!inst.has_privilege("alice", "shop", Privilege::Read).unwrap());
}

#[test]
fn parses_grant_and_revoke() {
    use chaoticdb::sql::ast::{GrantStmt, RevokeStmt, Stmt};
    use chaoticdb::sql::parser::parse;

    let one = |sql: &str| parse(sql).unwrap().remove(0);
    assert_eq!(
        one("grant read on shop to alice;"),
        Stmt::Grant(GrantStmt {
            privileges: vec![Privilege::Read],
            database: "shop".into(),
            user: "alice".into(),
        })
    );
    assert_eq!(
        one("grant read, write on shop to alice;"),
        Stmt::Grant(GrantStmt {
            privileges: vec![Privilege::Read, Privilege::Write],
            database: "shop".into(),
            user: "alice".into(),
        })
    );
    assert_eq!(
        one("grant all on * to alice;"),
        Stmt::Grant(GrantStmt {
            privileges: vec![Privilege::Read, Privilege::Write],
            database: "*".into(),
            user: "alice".into(),
        })
    );
    assert_eq!(
        one("revoke read on shop from alice;"),
        Stmt::Revoke(RevokeStmt {
            privileges: vec![Privilege::Read],
            database: "shop".into(),
            user: "alice".into(),
        })
    );
    assert!(parse("grant read shop to alice;").is_err());
}
