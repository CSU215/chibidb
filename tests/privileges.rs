use chibidb::ast::Privilege;
use chibidb::config::Config;
use chibidb::instance::Instance;
use chibidb::Session;

fn instance(dir: &tempfile::TempDir) -> Instance {
    Instance::open(dir.path(), &Config::default()).unwrap()
}

fn setup(inst: &mut Instance, s: &mut Session) {
    inst.execute_with(s, "create user alice identified by 'x';").unwrap();
    inst.execute_with(s, "create database shop;").unwrap();
}

#[test]
fn grant_and_check_read_privilege() {
    let dir = tempfile::tempdir().unwrap();
    let mut inst = instance(&dir);
    let mut s = Session::new();
    setup(&mut inst, &mut s);

    assert!(!inst.has_privilege("alice", "shop", Privilege::Read).unwrap());
    inst.execute_with(&mut s, "grant read on shop to alice;").unwrap();
    assert!(inst.has_privilege("alice", "shop", Privilege::Read).unwrap());
    assert!(!inst.has_privilege("alice", "shop", Privilege::Write).unwrap());
}

#[test]
fn grant_all_on_all_databases() {
    let dir = tempfile::tempdir().unwrap();
    let mut inst = instance(&dir);
    let mut s = Session::new();
    setup(&mut inst, &mut s);

    inst.execute_with(&mut s, "grant all on * to alice;").unwrap();
    assert!(inst.has_privilege("alice", "shop", Privilege::Read).unwrap());
    assert!(inst.has_privilege("alice", "other", Privilege::Write).unwrap());
    assert!(!inst.has_privilege("bob", "shop", Privilege::Read).unwrap());
}

#[test]
fn revoke_removes_the_privilege() {
    let dir = tempfile::tempdir().unwrap();
    let mut inst = instance(&dir);
    let mut s = Session::new();
    setup(&mut inst, &mut s);

    inst.execute_with(&mut s, "grant write on shop to alice;").unwrap();
    assert!(inst.has_privilege("alice", "shop", Privilege::Write).unwrap());
    inst.execute_with(&mut s, "revoke write on shop from alice;").unwrap();
    assert!(!inst.has_privilege("alice", "shop", Privilege::Write).unwrap());
}

#[test]
fn granting_to_unknown_user_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut inst = instance(&dir);
    let mut s = Session::new();
    setup(&mut inst, &mut s);
    assert!(inst.execute_with(&mut s, "grant read on shop to bob;").is_err());
}

#[test]
fn parses_grant_and_revoke() {
    use chibidb::ast::{GrantStmt, RevokeStmt, Stmt};
    use chibidb::parser::parse;

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
