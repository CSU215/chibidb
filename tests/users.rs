use chibidb::config::Config;
use chibidb::instance::Instance;
use chibidb::Session;

fn instance(dir: &tempfile::TempDir) -> Instance {
    Instance::open(dir.path(), &Config::default()).unwrap()
}

#[test]
fn create_and_authenticate_a_user() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);

    inst.create_user("alice", "secret").unwrap();
    assert!(inst.authenticate("alice", "secret").unwrap());
}

#[test]
fn wrong_password_or_unknown_user_fails() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    inst.create_user("alice", "secret").unwrap();

    assert!(!inst.authenticate("alice", "wrong").unwrap());
    assert!(!inst.authenticate("bob", "secret").unwrap());
}

#[test]
fn rejects_duplicate_and_invalid_user_names() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    inst.create_user("alice", "secret").unwrap();

    assert!(inst.create_user("alice", "other").is_err());
    assert!(inst.create_user("", "x").is_err());
    assert!(inst.create_user("bad name", "x").is_err());
}

#[test]
fn drop_user_revokes_authentication() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    inst.create_user("alice", "secret").unwrap();
    inst.drop_user("alice").unwrap();

    assert!(!inst.authenticate("alice", "secret").unwrap());
    assert!(inst.drop_user("alice").is_err());
}

#[test]
fn users_persist_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let inst = instance(&dir);
        inst.create_user("alice", "secret").unwrap();
    }
    let inst = instance(&dir);
    assert!(inst.authenticate("alice", "secret").unwrap());
}

#[test]
fn create_and_drop_user_via_sql() {
    let dir = tempfile::tempdir().unwrap();
    let inst = instance(&dir);
    let mut s = Session::new();

    inst.execute_with(&mut s, "create user alice identified by 'secret';")
        .unwrap();
    assert!(inst.authenticate("alice", "secret").unwrap());

    inst.execute_with(&mut s, "drop user alice;").unwrap();
    assert!(!inst.authenticate("alice", "secret").unwrap());
}

#[test]
fn parses_user_statements() {
    use chibidb::ast::{CreateUserStmt, DropUserStmt, Stmt};
    use chibidb::parser::parse;

    let one = |sql: &str| parse(sql).unwrap().remove(0);
    assert_eq!(
        one("create user alice identified by 'pw';"),
        Stmt::CreateUser(CreateUserStmt { name: "alice".into(), password: "pw".into() })
    );
    assert_eq!(
        one("create user 'bob' identified by 'x';"),
        Stmt::CreateUser(CreateUserStmt { name: "bob".into(), password: "x".into() })
    );
    assert_eq!(
        one("drop user alice;"),
        Stmt::DropUser(DropUserStmt { name: "alice".into() })
    );
    assert!(parse("create user alice;").is_err());
    assert!(parse("create user alice identified 'pw';").is_err());
}
