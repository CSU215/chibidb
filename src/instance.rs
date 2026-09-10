use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::ast::Stmt;
use crate::config::Config;
use crate::parser;
use crate::result::ResultSet;
use crate::trx::Session;
use crate::{Database, Error, Result};

/// Directory under the data root reserved for instance-wide metadata
/// (database catalogue, users, privileges). It is never a user database.
pub const META_DIR: &str = "chibi_meta";

/// Database selected when a session runs a table statement before choosing one.
pub const DEFAULT_DB: &str = "main";

/// One server instance: a data root holding multiple named databases, each in
/// its own directory (MySQL-style). Databases are opened lazily and cached.
pub struct Instance {
    config: Config,
    root: PathBuf,
    databases: HashMap<String, Database>,
    _temp: Option<tempfile::TempDir>,
}

impl Instance {
    /// Opens (creating if needed) the data root and its metadata directory.
    pub fn open(root: &Path, config: &Config) -> Result<Self> {
        std::fs::create_dir_all(root)
            .map_err(|e| Error::Runtime(format!("cannot create {}: {e}", root.display())))?;
        let meta = root.join(META_DIR);
        std::fs::create_dir_all(&meta)
            .map_err(|e| Error::Runtime(format!("cannot create {}: {e}", meta.display())))?;
        Ok(Self {
            config: config.clone(),
            root: root.to_path_buf(),
            databases: HashMap::new(),
            _temp: None,
        })
    }

    /// A throwaway instance in an automatically-cleaned temporary directory.
    pub fn open_in_memory(config: &Config) -> Result<Self> {
        let temp = tempfile::tempdir()
            .map_err(|e| Error::Runtime(format!("cannot create temp dir: {e}")))?;
        let inst = Self::open(temp.path(), config)?;
        Ok(Self { _temp: Some(temp), ..inst })
    }

    /// Names of all databases on disk, sorted. Purely directory-based so it
    /// reflects exactly what exists.
    pub fn databases(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in read_dir(&self.root)? {
            let name = entry;
            if name == META_DIR {
                continue;
            }
            names.push(name);
        }
        names.sort();
        Ok(names)
    }

    pub fn create_database(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        let path = self.root.join(name);
        if path.exists() {
            return Err(Error::Runtime(format!("database already exists: {name}")));
        }
        std::fs::create_dir_all(&path)
            .map_err(|e| Error::Runtime(format!("cannot create {}: {e}", path.display())))?;
        Ok(())
    }

    /// Removes a database directory. The cached handle is dropped first so its
    /// files are closed before deletion (required on Windows).
    pub fn drop_database(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        self.databases.remove(name);
        let path = self.root.join(name);
        if !path.is_dir() {
            return Err(Error::Runtime(format!("no such database: {name}")));
        }
        std::fs::remove_dir_all(&path)
            .map_err(|e| Error::Runtime(format!("cannot remove {}: {e}", path.display())))?;
        Ok(())
    }

    /// Returns the database, opening and caching it on first use.
    pub fn database_mut(&mut self, name: &str) -> Result<&mut Database> {
        validate_name(name)?;
        if !self.databases.contains_key(name) {
            let path = self.root.join(name);
            if !path.is_dir() {
                return Err(Error::Runtime(format!("no such database: {name}")));
            }
            let db = Database::open_with_config(&path, &self.config)?;
            self.databases.insert(name.to_string(), db);
        }
        Ok(self.databases.get_mut(name).expect("database was just opened"))
    }

    /// Top-level SQL entry. Database-level statements (`CREATE/DROP DATABASE`,
    /// `USE`) are handled here; everything else is routed to the session's
    /// current database.
    pub fn execute_with(&mut self, session: &mut Session, sql: &str) -> Result<Vec<ResultSet>> {
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
                other => {
                    let db_name = self.ensure_current_db(session)?;
                    let db = self.database_mut(&db_name)?;
                    if let Some(rs) = db.execute_stmt_with(session, other)? {
                        out.push(rs);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Returns the current database name, creating and selecting the default
    /// one when the session has not chosen one yet.
    fn ensure_current_db(&mut self, session: &mut Session) -> Result<String> {
        if let Some(name) = session.current_db() {
            return Ok(name.to_string());
        }
        if !self.databases()?.iter().any(|d| d == DEFAULT_DB) {
            self.create_database(DEFAULT_DB)?;
        }
        session.set_current_db(Some(DEFAULT_DB.to_string()));
        Ok(DEFAULT_DB.to_string())
    }

    /// Flushes (checkpoints) every currently open database. Used on clean
    /// shutdown so the WAL is truncated.
    pub fn flush(&mut self) -> Result<()> {
        for db in self.databases.values_mut() {
            db.flush()?;
        }
        Ok(())
    }

    /// Rolls back a session's open transaction, if any, on its current db.
    pub fn rollback_session(&mut self, session: &mut Session) -> Result<()> {
        let Some(name) = session.current_db().map(str::to_string) else {
            return Ok(());
        };
        if self.root.join(&name).is_dir() {
            self.database_mut(&name)?.rollback_session(session)?;
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
}

fn read_dir(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)
        .map_err(|e| Error::Runtime(format!("cannot read {}: {e}", dir.display())))?
    {
        let entry = entry
            .map_err(|e| Error::Runtime(format!("cannot read {}: {e}", dir.display())))?;
        if !entry.path().is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

fn validate_name(name: &str) -> Result<()> {
    if name == META_DIR {
        return Err(Error::Runtime(format!("database name is reserved: {name}")));
    }
    let valid = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        return Err(Error::Runtime(format!("invalid database name: {name}")));
    }
    Ok(())
}
