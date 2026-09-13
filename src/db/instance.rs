use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::ast::{DataType, Privilege, Stmt};
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

/// A read-only virtual database exposing instance metadata. It has no
/// directory; `USE` selects it and queries run against materialized system
/// tables.
pub const INFORMATION_SCHEMA: &str = "information_schema";

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
            // `password` is the login hash; `native` is the SHA1-based verifier
            // the MySQL frontend needs for mysql_native_password.
            meta.execute_sql(
                "create table users (name char(64) primary key, password char(128) not null, native char(40) not null);",
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
        // Remove grants on this database so a same-named one cannot inherit them.
        let meta = self.meta.write();
        delete_privileges(&meta, &format!("dbname = '{name}'"))?;
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
            self.authorize(session, stmt)?;
            match stmt {
                Stmt::Login(l) => {
                    self.reject_in_trx(session)?;
                    self.login(session, &l.name, &l.password)?;
                    out.push(ResultSet::Message("SUCCESS".into()));
                }
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
                    if session.current_db() == Some(INFORMATION_SCHEMA) {
                        out.push(self.query_information_schema(session, other)?);
                        continue;
                    }
                    let db_name = self.ensure_current_db(session)?;
                    let db = self.database(&db_name)?;
                    let read_only = crate::is_read_only(other);
                    // Take the 2PL write lock before the database lock. The
                    // clone is taken under a short read, then the wait happens
                    // with no database lock held, so it cannot deadlock against
                    // another session's COMMIT.
                    let pre_acquired = self.config.transaction.conflict
                        == crate::config::ConflictStrategy::TwoPl
                        && !read_only
                        && !session.holds_writer();
                    let write_lock = if pre_acquired {
                        Some(db.read().write_lock())
                    } else {
                        None
                    };
                    if let Some(lock) = &write_lock {
                        lock.acquire(session.id())?;
                    }
                    // read-only statements share the database; anything that
                    // may write takes it exclusively for the statement
                    let result = if read_only {
                        db.read().execute_stmt_with(session, other)?
                    } else {
                        db.write().execute_stmt_with(session, other)?
                    };
                    // release only if this statement took it and no explicit
                    // transaction (BEGIN) adopted it
                    if let Some(lock) = &write_lock
                        && !session.holds_writer()
                    {
                        lock.release(session.id());
                    }
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
        if name == INFORMATION_SCHEMA {
            session.set_current_db(Some(name.to_string()));
            return Ok(());
        }
        if !self.databases()?.iter().any(|d| d == name) {
            return Err(Error::Runtime(format!("no such database: {name}")));
        }
        session.set_current_db(Some(name.to_string()));
        Ok(())
    }

    /// Runs a read-only statement against a freshly materialized copy of the
    /// instance metadata (`schemata`, `tables`, `columns`).
    fn query_information_schema(&self, session: &mut Session, stmt: &Stmt) -> Result<ResultSet> {
        if !matches!(stmt, Stmt::Select(_) | Stmt::Explain(_)) {
            return Err(Error::Runtime(
                "information_schema is read-only (select only)".into(),
            ));
        }
        let tmp = Database::open_in_memory_with_config(&self.config)?;
        self.materialize_information_schema(&tmp)?;
        tmp.execute_stmt_with(session, stmt)?
            .ok_or_else(|| Error::Runtime("information_schema query produced no result".into()))
    }

    fn materialize_information_schema(&self, db: &Database) -> Result<()> {
        db.execute_sql("create table schemata (schema_name char(64) primary key);")?;
        db.execute_sql(
            "create table tables (table_schema char(64), table_name char(64), engine char(8), \
             page_layout char(8));",
        )?;
        db.execute_sql(
            "create table columns (table_schema char(64), table_name char(64), \
             column_name char(64), data_type char(32), not_null int, primary_key int, is_unique int);",
        )?;
        for name in self.databases()? {
            db.execute_sql(&format!("insert into schemata values ('{name}');"))?;
            let handle = self.database(&name)?;
            let guard = handle.read();
            for meta in guard.catalog().table_metas() {
                let engine = match meta.engine {
                    crate::config::EngineKind::Heap => "heap",
                    crate::config::EngineKind::Lsm => "lsm",
                };
                let layout = match meta.layout {
                    crate::config::PageLayout::Row => "row",
                    crate::config::PageLayout::Pax => "pax",
                };
                db.execute_sql(&format!(
                    "insert into tables values ('{name}', '{}', '{engine}', '{layout}');",
                    meta.name
                ))?;
                for c in &meta.columns {
                    db.execute_sql(&format!(
                        "insert into columns values ('{name}', '{}', '{}', '{}', {}, {}, {});",
                        meta.name,
                        c.name,
                        dtype_name(c.dtype),
                        c.not_null as i32,
                        c.primary_key as i32,
                        c.unique as i32,
                    ))?;
                }
            }
        }
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

    /// Authenticates `user` and binds it to the session.
    pub fn login(&self, session: &mut Session, user: &str, password: &str) -> Result<()> {
        if !self.authenticate(user, password)? {
            return Err(Error::Runtime("authentication failed".into()));
        }
        session.set_user(Some(user.to_string()));
        Ok(())
    }

    /// Enforces authentication and per-database privileges when
    /// `auth.enabled`. Without it every statement is allowed (as before).
    fn authorize(&self, session: &Session, stmt: &Stmt) -> Result<()> {
        if !self.config.auth.enabled || matches!(stmt, Stmt::Login(_)) {
            return Ok(());
        }
        // bootstrap: the first account may be created before anyone logs in
        if session.user().is_none()
            && matches!(stmt, Stmt::CreateUser(_))
            && !self.any_users()?
        {
            return Ok(());
        }
        let Some(user) = session.user() else {
            return Err(Error::Runtime("not logged in".into()));
        };
        // account administration only needs a login (there is no role model)
        if matches!(
            stmt,
            Stmt::CreateUser(_)
                | Stmt::DropUser(_)
                | Stmt::Grant(_)
                | Stmt::Revoke(_)
                | Stmt::CreateDatabase(_)
                | Stmt::DropDatabase(_)
        ) {
            return Ok(());
        }
        // the virtual metadata database is readable by any logged-in user
        if session.current_db() == Some(INFORMATION_SCHEMA) {
            return Ok(());
        }
        let Some(privilege) = statement_privilege(stmt) else {
            return Ok(());
        };
        let db = session.current_db().unwrap_or(DEFAULT_DB);
        if !self.has_privilege(user, db, privilege)? {
            return Err(Error::Runtime(format!(
                "permission denied for {user} on {db}"
            )));
        }
        Ok(())
    }

    fn any_users(&self) -> Result<bool> {
        let meta = self.meta.read();
        let result = meta.execute_sql("select count(*) from users;")?;
        Ok(match result.into_iter().next() {
            Some(ResultSet::Rows { rows, .. }) if !rows.is_empty() => {
                matches!(rows[0].first(), Some(Value::Int(n)) if *n > 0)
            }
            _ => false,
        })
    }

    // ---- users -----------------------------------------------------------

    pub fn create_user(&self, name: &str, password: &str) -> Result<()> {
        validate_ident(name, "user name")?;
        let meta = self.meta.write();
        if user_exists(&meta, name)? {
            return Err(Error::Runtime(format!("user already exists: {name}")));
        }
        let hash = hash_password(password);
        let native = crate::mysql::native_verifier_hex(password);
        meta.execute_sql(&format!(
            "insert into users values ('{name}', '{hash}', '{native}');"
        ))?;
        Ok(())
    }

    /// The stored MySQL `mysql_native_password` verifier for `name`, if the
    /// user exists.
    pub fn native_verifier(&self, name: &str) -> Result<Option<String>> {
        validate_ident(name, "user name")?;
        let meta = self.meta.read();
        let result =
            meta.execute_sql(&format!("select native from users where name = '{name}';"))?;
        Ok(match result.into_iter().next() {
            Some(ResultSet::Rows { mut rows, .. }) if !rows.is_empty() => {
                match rows.remove(0).into_iter().next() {
                    Some(Value::Str(v)) => Some(v),
                    _ => None,
                }
            }
            _ => None,
        })
    }

    pub fn drop_user(&self, name: &str) -> Result<()> {
        validate_ident(name, "user name")?;
        let meta = self.meta.write();
        if !user_exists(&meta, name)? {
            return Err(Error::Runtime(format!("no such user: {name}")));
        }
        meta.execute_sql(&format!("delete from users where name = '{name}';"))?;
        // Drop the user's grants too, so recreating the name cannot inherit them.
        delete_privileges(&meta, &format!("username = '{name}'"))?;
        Ok(())
    }

    /// True when `password` matches the stored hash for `name`.
    pub fn authenticate(&self, name: &str, password: &str) -> Result<bool> {
        validate_ident(name, "user name")?;
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

/// The SQL name of a column type, for `information_schema.columns`.
fn dtype_name(dtype: DataType) -> String {
    match dtype {
        DataType::Int => "int".into(),
        DataType::Float => "float".into(),
        DataType::Char(n) => format!("char({n})"),
        DataType::Date => "date".into(),
        DataType::Text => "text".into(),
    }
}

/// The privilege a statement needs on its current database, if any.
fn statement_privilege(stmt: &Stmt) -> Option<Privilege> {
    match stmt {
        Stmt::Select(_) | Stmt::Explain(_) => Some(Privilege::Read),
        Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) => Some(Privilege::Write),
        Stmt::CreateTable(_)
        | Stmt::CreateIndex(_)
        | Stmt::CreateView(_)
        | Stmt::DropTable(_)
        | Stmt::DropIndex(_)
        | Stmt::DropView(_)
        | Stmt::Checkpoint
        | Stmt::Vacuum => Some(Privilege::Write),
        _ => None,
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

/// Deletes every privilege row matching `predicate` (already SQL-escaped).
fn delete_privileges(meta: &Database, predicate: &str) -> Result<()> {
    meta.execute_sql(&format!("delete from privileges where {predicate};"))?;
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
