use chaoticdb::config::Config;
use chaoticdb::instance::Instance;
use chaoticdb::Session;

fn auth_config() -> Config {
    let mut cfg = Config::default();
    cfg.auth.enabled = true;
    cfg
}

fn err(inst: &Instance, session: &mut Session, sql: &str) -> String {
    inst.execute_with(session, sql).unwrap_err().to_string()
}

#[test]
fn auth_is_enforced_when_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let inst = Instance::open(dir.path(), &auth_config()).unwrap();
    let mut s = Session::new();

    // not logged in: ordinary statements are refused
    assert!(err(&inst, &mut s, "select 1;").contains("not logged in"));

    // bootstrap: the first account can be created before anyone logs in
    inst.execute_with(&mut s, "create user alice identified by 'pw';").unwrap();

    // authentication is checked
    assert!(
        err(&inst, &mut s, "login alice identified by 'nope';")
            .contains("authentication failed")
    );
    inst.execute_with(&mut s, "login alice identified by 'pw';").unwrap();

    // logged in, but with no grants: DDL is denied
    inst.execute_with(&mut s, "create database shop;").unwrap();
    inst.execute_with(&mut s, "use shop;").unwrap();
    assert!(err(&inst, &mut s, "create table t (id int);").contains("permission denied"));

    // grant write, then DDL and DML work
    inst.execute_with(&mut s, "grant write on shop to alice;").unwrap();
    inst.execute_with(&mut s, "create table t (id int);").unwrap();
    inst.execute_with(&mut s, "insert into t values (1);").unwrap();

    // reads still need the read privilege
    assert!(err(&inst, &mut s, "select * from t;").contains("permission denied"));
    inst.execute_with(&mut s, "grant read on shop to alice;").unwrap();
    inst.execute_with(&mut s, "select * from t;").unwrap();
}

#[test]
fn a_read_only_user_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let inst = Instance::open(dir.path(), &auth_config()).unwrap();

    let mut admin = Session::new();
    inst.execute_with(&mut admin, "create user alice identified by 'pw';").unwrap();
    inst.execute_with(&mut admin, "login alice identified by 'pw';").unwrap();
    inst.execute_with(&mut admin, "create database shop;").unwrap();
    inst.execute_with(&mut admin, "grant read on shop to alice;").unwrap();

    let mut alice = Session::new();
    inst.execute_with(&mut alice, "login alice identified by 'pw';").unwrap();
    inst.execute_with(&mut alice, "use shop;").unwrap();
    // cannot create a table (write)
    assert!(err(&inst, &mut alice, "create table t (id int);").contains("permission denied"));
}

#[test]
fn auth_disabled_allows_everything() {
    let dir = tempfile::tempdir().unwrap();
    let inst = Instance::open(dir.path(), &Config::default()).unwrap();
    let mut s = Session::new();
    inst.execute_with(&mut s, "create database shop;").unwrap();
    inst.execute_with(&mut s, "use shop;").unwrap();
    inst.execute_with(&mut s, "create table t (id int);").unwrap();
    inst.execute_with(&mut s, "insert into t values (1);").unwrap();
}

#[test]
fn injected_user_name_is_rejected_not_executed() {
    let dir = tempfile::tempdir().unwrap();
    let inst = Instance::open(dir.path(), &auth_config()).unwrap();
    let mut s = Session::new();
    inst.execute_with(&mut s, "create user alice identified by 'pw';").unwrap();

    // A quoted name used to be interpolated straight into the metadata query.
    let injected = "x' or '1'='1";
    assert!(inst.authenticate(injected, "pw").unwrap_err().to_string().contains("invalid"));
    assert!(inst.native_verifier(injected).unwrap_err().to_string().contains("invalid"));

    // the users table is intact: alice still authenticates
    assert!(inst.authenticate("alice", "pw").unwrap());
}

#[test]
fn checkpoint_and_vacuum_require_write_privilege() {
    let dir = tempfile::tempdir().unwrap();
    let inst = Instance::open(dir.path(), &auth_config()).unwrap();
    let mut s = Session::new();
    inst.execute_with(&mut s, "create user alice identified by 'pw';").unwrap();
    inst.execute_with(&mut s, "login alice identified by 'pw';").unwrap();
    inst.execute_with(&mut s, "create database shop;").unwrap();
    inst.execute_with(&mut s, "grant read on shop to alice;").unwrap();
    inst.execute_with(&mut s, "use shop;").unwrap();

    // read alone must not allow maintenance statements
    assert!(err(&inst, &mut s, "checkpoint;").contains("permission denied"));
    assert!(err(&inst, &mut s, "vacuum;").contains("permission denied"));
}

#[test]
fn manage_is_required_to_administer_users_and_grants() {
    let dir = tempfile::tempdir().unwrap();
    let inst = Instance::open(dir.path(), &auth_config()).unwrap();

    // the bootstrap account is the administrator
    let mut admin = Session::new();
    inst.execute_with(&mut admin, "create user admin identified by 'pw';").unwrap();
    inst.execute_with(&mut admin, "login admin identified by 'pw';").unwrap();
    inst.execute_with(&mut admin, "create user bob identified by 'pw';").unwrap();
    inst.execute_with(&mut admin, "grant read on shop to bob;").unwrap();

    // bob can log in but cannot manage accounts or hand out privileges
    let mut bob = Session::new();
    inst.execute_with(&mut bob, "login bob identified by 'pw';").unwrap();
    assert!(err(&inst, &mut bob, "create user carol identified by 'pw';").contains("manage required"));
    assert!(err(&inst, &mut bob, "drop user admin;").contains("manage required"));
    assert!(err(&inst, &mut bob, "grant write on shop to bob;").contains("manage required"));
}

#[test]
fn manage_can_be_scoped_to_one_database() {
    let dir = tempfile::tempdir().unwrap();
    let inst = Instance::open(dir.path(), &auth_config()).unwrap();

    let mut admin = Session::new();
    inst.execute_with(&mut admin, "create user admin identified by 'pw';").unwrap();
    inst.execute_with(&mut admin, "login admin identified by 'pw';").unwrap();
    inst.execute_with(&mut admin, "create user mallory identified by 'pw';").unwrap();
    inst.execute_with(&mut admin, "grant manage on shop to mallory;").unwrap();

    let mut mallory = Session::new();
    inst.execute_with(&mut mallory, "login mallory identified by 'pw';").unwrap();

    // manage on shop lets mallory grant on shop ...
    inst.execute_with(&mut mallory, "grant read on shop to admin;").unwrap();
    // ... but not elsewhere, and not administer accounts
    assert!(
        err(&inst, &mut mallory, "grant read on other to admin;").contains("manage required")
    );
    assert!(
        err(&inst, &mut mallory, "create user carol identified by 'pw';")
            .contains("manage required")
    );
}
