use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::ast::{Privilege, Stmt};
use crate::config::Config;
use crate::parser;
use crate::result::ResultSet;
use crate::trx::Session;
use crate::value::Value;
use crate::{Database, Error, Result};

/// Directory under the data root reserved for instance-wide metadata
/// (database catalogue, users, privileges). It is never a user database.
pub const META_DIR: &str = "chibi_meta";

/// Database selected when a session runs a table statement before choosing one.
pub const DEFAULT_DB: &str = "main";

/// One server instance: a data root holding multiple named databases, each in
/// its own directory. Each database has its own lock, so different databases
/// can be used concurrently; statements on the same database serialize.
pub struct Instance {
    config: Config,
    root: PathBuf,
    databases: RwLock<HashMap<String, Arc<RwLock<Database>>>>,
    meta: Arc<RwLock<Database>>,
    _temp: Option<tempfile::TempDir>,
}

impl Instance {
    /// Opens (creating if needed) the data root and its metadata directory,
    /// then bootstraps the system tables.
    pub fn open(root: &Path, config: &Config) -> Result<Self> {
        std::fs::create_dir_all(root)
            .map_err(|e| Error::Runtime(format!("cannot create {}: {e}", root.display())))?;
        let meta_path = root.join(META_DIR);
        std::fs::create_dir_all(&meta_path)
            .map_err(|e| Error::Runtime(format!("cannot create {}: {e}", meta_path.display())))?;
        let meta = Arc::new(RwLock::new(Database::open_with_config(&meta_path, config)?));
        let instance = Self {
            config: config.clone(),
            root: root.to_path_buf(),
            databases: RwLock::new(HashMap::new()),
            meta,
            _temp: None,
        };
        instance.bootstrap_meta()?;
        Ok(instance)
    }

    /// A throwaway instance in an automatically-cleaned temporary directory.
    pub fn open_in_memory(config: &Config) -> Result<Self> {
        let temp = tempfile::tempdir()
            .map_err(|e| Error::Runtime(format!("cannot create temp dir: {e}")))?;
        let inst = Self::open(temp.path(), config)?;
        Ok(Self { _temp: Some(temp), ..inst })
    }

    /// The instance configuration (used by the server to pick a thread model).
    pub fn config(&self) -> &Config {
        &self.config
    }

    fn bootstrap_meta(&self) -> Result<()> {
        let meta = self.meta.read();
        if !meta.table_exists("databases") {
            meta.execute_sql("create table databases (name char(64) primary key);")?;
        }
        if !meta.table_exists("users") {
            meta.execute_sql(
                "create table users (name char(64) primary key, password char(128) not null);",
            )?;
        }
        if !meta.table_exists("privileges") {
            meta.execute_sql(
                "create table privileges (username char(64), dbname char(64), kind char(16));",
            )?;
        }
        Ok(())
    }

    /// Names of registered databases, sorted. The `databases` system table is
    /// the source of truth, so a stray directory is not a database.
    pub fn databases(&self) -> Result<Vec<String>> {
        let meta = self.meta.read();
        let result = meta.execute_sql("select name from databases;")?;
        let mut names = Vec::new();
        if let Some(ResultSet::Rows { rows, .. }) = result.into_iter().next() {
            for row in rows {
                match row.into_iter().next() {
                    Some(Value::Str(name)) => names.push(name),
                    other => {
                        return Err(Error::Runtime(format!(
                            "corrupt database registry: {other:?}"
                        )))
                    }
                }
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn create_database(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        if self.databases()?.iter().any(|d| d == name) {
            return Err(Error::Runtime(format!("database already exists: {name}")));
        }
        let path = self.root.join(name);
        std::fs::create_dir_all(&path)
            .map_err(|e| Error::Runtime(format!("cannot create {}: {e}", path.display())))?;
        self.meta
            .write()
            .execute_sql(&format!("insert into databases values ('{name}');"))?;
        Ok(())
    }

    /// Removes a database directory. The cached handle is dropped first so its
    /// files are closed before deletion (required on Windows).
    pub fn drop_database(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        self.databases.write().remove(name);
        let path = self.root.join(name);
        if !path.is_dir() {
            return Err(Error::Runtime(format!("no such database: {name}")));
        }
        std::fs::remove_dir_all(&path)
            .map_err(|e| Error::Runtime(format!("cannot remove {}: {e}", path.display())))?;
        self.meta
            .write()
            .execute_sql(&format!("delete from databases where name = '{name}';"))?;
        Ok(())
    }

    /// Returns a handle to the database, opening and caching it on first use.
    pub fn database(&self, name: &str) -> Result<Arc<RwLock<Database>>> {
        validate_name(name)?;
        if let Some(db) = self.databases.read().get(name) {
            return Ok(db.clone());
        }
        let path = self.root.join(name);
        if !path.is_dir() {
            return Err(Error::Runtime(format!("no such database: {name}")));
        }
        let mut map = self.databases.write();
        if let Some(db) = map.get(name) {
            return Ok(db.clone());
        }
        let db = Arc::new(RwLock::new(Database::open_with_config(&path, &self.config)?));
        map.insert(name.to_string(), db.clone());
        Ok(db)
    }

    /// Takes the database's write lock and runs `f` against it.
    pub fn with_database_mut<R>(
        &self,
        name: &str,
        f: impl FnOnce(&mut Database) -> R,
    ) -> Result<R> {
        let db = self.database(name)?;
        let mut guard = db.write();
        Ok(f(&mut guard))
    }

    /// Top-level SQL entry. Database-level statements (`CREATE/DROP DATABASE`,
    /// `USE`) and user/privilege statements are handled here; everything else
    /// is routed to the session's current database under its lock.
    pub fn execute_with(&self, session: &mut Session, sql: &str) -> Result<Vec<ResultSet>> {
        let stmts = parser::parse(sql)?;
        let mut out = Vec::new();
        for stmt in &stmts {
            match stmt {
                Stmt::CreateDatabase(c) => {
                    self.reject_in_trx(session)?;
                    self.create_database(&c.name)?;
                    out.push(ResultSet::Message("SUCCESS".into()));
                }
                Stmt::DropDatabase(d) => {
                    self.reject_in_trx(session)?;
                    self.drop_database(&d.name)?;
                    out.push(ResultSet::Message("SUCCESS".into()));
                }
                Stmt::Use(u) => {
                    self.reject_in_trx(session)?;
                    self.use_database(session, &u.name)?;
                    out.push(ResultSet::Message("SUCCESS".into()));
                }
                Stmt::CreateUser(c) => {
                    self.reject_in_trx(session)?;
                    self.create_user(&c.name, &c.password)?;
                    out.push(ResultSet::Message("SUCCESS".into()));
                }
                Stmt::DropUser(d) => {
                    self.reject_in_trx(session)?;
                    self.drop_user(&d.name)?;
                    out.push(ResultSet::Message("SUCCESS".into()));
                }
                Stmt::Grant(g) => {
                    self.reject_in_trx(session)?;
                    for privilege in &g.privileges {
                        self.grant(&g.user, &g.database, *privilege)?;
                    }
                    out.push(ResultSet::Message("SUCCESS".into()));
                }
                Stmt::Revoke(r) => {
                    self.reject_in_trx(session)?;
                    for privilege in &r.privileges {
                        self.revoke(&r.user, &r.database, *privilege)?;
                    }
                    out.push(ResultSet::Message("SUCCESS".into()));
                }
                other => {
                    let db_name = self.ensure_current_db(session)?;
                    let db = self.database(&db_name)?;
                    // read-only statements share the database; anything that
                    // may write takes it exclusively for the statement
                    let result = if crate::is_read_only(other) {
                        db.read().execute_stmt_with(session, other)?
                    } else {
                        db.write().execute_stmt_with(session, other)?
                    };
                    if let Some(rs) = result {
                        out.push(rs);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Returns the current database name, creating and selecting the default
    /// one when the session has not chosen one yet.
    fn ensure_current_db(&self, session: &mut Session) -> Result<String> {
        if let Some(name) = session.current_db() {
            return Ok(name.to_string());
        }
        if !self.databases()?.iter().any(|d| d == DEFAULT_DB) {
            self.create_database(DEFAULT_DB)?;
        }
        session.set_current_db(Some(DEFAULT_DB.to_string()));
        Ok(DEFAULT_DB.to_string())
    }

    /// Flushes (checkpoints) the system database and every open database.
    pub fn flush(&self) -> Result<()> {
        self.meta.read().flush()?;
        let dbs: Vec<Arc<RwLock<Database>>> =
            self.databases.read().values().cloned().collect();
        for db in dbs {
            db.read().flush()?;
        }
        Ok(())
    }

    /// Rolls back a session's open transaction, if any, on its current db.
    pub fn rollback_session(&self, session: &mut Session) -> Result<()> {
        let Some(name) = session.current_db().map(str::to_string) else {
            return Ok(());
        };
        if self.root.join(&name).is_dir() {
            self.database(&name)?.write().rollback_session(session)?;
        } else {
            // the database was dropped out from under the session; its undo
            // log references gone tables, so just drop the handle
            session.trx = None;
        }
        Ok(())
    }

    fn use_database(&self, session: &mut Session, name: &str) -> Result<()> {
        if !self.databases()?.iter().any(|d| d == name) {
            return Err(Error::Runtime(format!("no such database: {name}")));
        }
        session.set_current_db(Some(name.to_string()));
        Ok(())
    }

    fn reject_in_trx(&self, session: &Session) -> Result<()> {
        if session.trx.is_some() {
            return Err(Error::Runtime(
                "cannot run database statements inside a transaction".into(),
            ));
        }
        Ok(())
    }

    // ---- users -----------------------------------------------------------

    pub fn create_user(&self, name: &str, password: &str) -> Result<()> {
        validate_ident(name, "user name")?;
        let meta = self.meta.write();
        if user_exists(&meta, name)? {
            return Err(Error::Runtime(format!("user already exists: {name}")));
        }
        let hash = hash_password(password);
        meta.execute_sql(&format!("insert into users values ('{name}', '{hash}');"))?;
        Ok(())
    }

    pub fn drop_user(&self, name: &str) -> Result<()> {
        validate_ident(name, "user name")?;
        let meta = self.meta.write();
        if !user_exists(&meta, name)? {
            return Err(Error::Runtime(format!("no such user: {name}")));
        }
        meta.execute_sql(&format!("delete from users where name = '{name}';"))?;
        Ok(())
    }

    /// True when `password` matches the stored hash for `name`.
    pub fn authenticate(&self, name: &str, password: &str) -> Result<bool> {
        let meta = self.meta.read();
        let result =
            meta.execute_sql(&format!("select password from users where name = '{name}';"))?;
        let stored = match result.into_iter().next() {
            Some(ResultSet::Rows { mut rows, .. }) if !rows.is_empty() => {
                match rows.remove(0).into_iter().next() {
                    Some(Value::Str(hash)) => hash,
                    _ => return Ok(false),
                }
            }
            _ => return Ok(false),
        };
        Ok(verify_password(password, &stored))
    }

    // ---- privileges ------------------------------------------------------

    /// Grants one privilege to `user` on `database` (`*` = all databases).
    pub fn grant(&self, user: &str, database: &str, privilege: Privilege) -> Result<()> {
        validate_ident(user, "user name")?;
        validate_scope(database)?;
        let meta = self.meta.write();
        if !user_exists(&meta, user)? {
            return Err(Error::Runtime(format!("no such user: {user}")));
        }
        let kind = privilege.as_str();
        revoke(&meta, user, database, kind)?;
        meta.execute_sql(&format!(
            "insert into privileges values ('{user}', '{database}', '{kind}');"
        ))?;
        Ok(())
    }

    pub fn revoke(&self, user: &str, database: &str, privilege: Privilege) -> Result<()> {
        validate_ident(user, "user name")?;
        validate_scope(database)?;
        revoke(&self.meta.write(), user, database, privilege.as_str())
    }

    /// True when `user` holds `privilege` on `database` or on all databases.
    pub fn has_privilege(
        &self,
        user: &str,
        database: &str,
        privilege: Privilege,
    ) -> Result<bool> {
        validate_ident(user, "user name")?;
        validate_scope(database)?;
        let kind = privilege.as_str();
        let meta = self.meta.read();
        let result = meta.execute_sql(&format!(
            "select kind from privileges where username = '{user}' and kind = '{kind}' and (dbname = '{database}' or dbname = '*');"
        ))?;
        Ok(matches!(result.into_iter().next(), Some(ResultSet::Rows { rows, .. }) if !rows.is_empty()))
    }
}

fn user_exists(meta: &Database, name: &str) -> Result<bool> {
    let result = meta.execute_sql(&format!("select name from users where name = '{name}';"))?;
    Ok(matches!(result.into_iter().next(), Some(ResultSet::Rows { rows, .. }) if !rows.is_empty()))
}

fn revoke(meta: &Database, user: &str, database: &str, kind: &str) -> Result<()> {
    meta.execute_sql(&format!(
        "delete from privileges where username = '{user}' and dbname = '{database}' and kind = '{kind}';"
    ))?;
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name == META_DIR {
        return Err(Error::Runtime(format!("database name is reserved: {name}")));
    }
    validate_ident(name, "database name")
}

fn validate_ident(name: &str, what: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        return Err(Error::Runtime(format!("invalid {what}: {name}")));
    }
    Ok(())
}

/// A privilege scope is a database name or `*` (all databases).
fn validate_scope(database: &str) -> Result<()> {
    if database == "*" {
        return Ok(());
    }
    validate_ident(database, "database name")
}

/// Hashes `password` with a random 64-bit salt; stored as `salt$sha256`.
fn hash_password(password: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::hash::{BuildHasher, Hasher};

    let salt = std::hash::RandomState::new().build_hasher().finish();
    let mut hasher = Sha256::new();
    hasher.update(salt.to_le_bytes());
    hasher.update(password.as_bytes());
    format!("{salt:016x}${:x}", hasher.finalize())
}

fn verify_password(password: &str, stored: &str) -> bool {
    use sha2::{Digest, Sha256};

    let Some((salt_hex, expected)) = stored.split_once('$') else {
        return false;
    };
    let Ok(salt) = u64::from_str_radix(salt_hex, 16) else {
        return false;
    };
    let mut hasher = Sha256::new();
    hasher.update(salt.to_le_bytes());
    hasher.update(password.as_bytes());
    format!("{:x}", hasher.finalize()) == expected
}
