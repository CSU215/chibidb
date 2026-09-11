use chibidb::config::Config;
use chibidb::instance::Instance;

fn instance(dir: &tempfile::TempDir) -> Instance {
    Instance::open(dir.path(), &Config::default()).unwrap()
}

#[test]
fn create_and_authenticate_a_user() {
    let dir = tempfile::tempdir().unwrap();
    let mut inst = instance(&dir);

    inst.create_user("alice", "secret").unwrap();
    assert!(inst.authenticate("alice", "secret").unwrap());
}

#[test]
fn wrong_password_or_unknown_user_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mut inst = instance(&dir);
    inst.create_user("alice", "secret").unwrap();

    assert!(!inst.authenticate("alice", "wrong").unwrap());
    assert!(!inst.authenticate("bob", "secret").unwrap());
}

#[test]
fn rejects_duplicate_and_invalid_user_names() {
    let dir = tempfile::tempdir().unwrap();
    let mut inst = instance(&dir);
    inst.create_user("alice", "secret").unwrap();

    assert!(inst.create_user("alice", "other").is_err());
    assert!(inst.create_user("", "x").is_err());
    assert!(inst.create_user("bad name", "x").is_err());
}

#[test]
fn drop_user_revokes_authentication() {
    let dir = tempfile::tempdir().unwrap();
    let mut inst = instance(&dir);
    inst.create_user("alice", "secret").unwrap();
    inst.drop_user("alice").unwrap();

    assert!(!inst.authenticate("alice", "secret").unwrap());
    assert!(inst.drop_user("alice").is_err());
}

#[test]
fn users_persist_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut inst = instance(&dir);
        inst.create_user("alice", "secret").unwrap();
    }
    let mut inst = instance(&dir);
    assert!(inst.authenticate("alice", "secret").unwrap());
}
