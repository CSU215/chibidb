use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::{Database, Error, Result};

/// Directory under the data root reserved for instance-wide metadata
/// (database catalogue, users, privileges). It is never a user database.
pub const META_DIR: &str = "chibi_meta";

/// One server instance: a data root holding multiple named databases, each in
/// its own directory (MySQL-style). Databases are opened lazily and cached.
pub struct Instance {
    config: Config,
    root: PathBuf,
    databases: HashMap<String, Database>,
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
        })
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
