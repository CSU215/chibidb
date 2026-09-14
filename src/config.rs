use std::path::Path;

use serde::Deserialize;

use crate::{Error, Result};

/// Which storage engine new tables use. Existing files record their own
/// engine in the file header, so this only affects creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    #[default]
    Heap,
    Lsm,
}

/// Default execution model. Chunk (columnar batches) is the default; Volcano
/// (row-at-a-time) remains selectable and is the reference the differential
/// tests compare against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    Volcano,
    #[default]
    Chunk,
}

/// Physical page layout for heap tables. `Row` is the slotted record layout;
/// `Pax` stores a page as a column-major row group so a scan reads only the
/// columns it needs. Only heap tables support `Pax`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PageLayout {
    #[default]
    Row,
    Pax,
}

/// Transaction isolation level, following PostgreSQL's model. The difference
/// is only *when* a snapshot is taken and how a write-write conflict is
/// resolved: read committed refreshes the snapshot per statement and re-reads
/// the conflicting row (EPQ); repeatable read keeps one snapshot for the whole
/// transaction and aborts on a conflicting write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// Each statement sees the latest committed data; writers whose row was
    /// concurrently updated restart the statement and apply to the new version.
    #[default]
    ReadCommitted,
    /// The transaction sees one stable snapshot; a conflicting writer gets a
    /// serialization failure (SQLSTATE 40001).
    RepeatableRead,
    /// Snapshot isolation plus SSI conflict tracking: a transaction that would
    /// close a cycle of read/write dependencies is aborted (SQLSTATE 40001).
    Serializable,
}

/// How accepted connections are mapped to execution threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ThreadModel {
    /// One dedicated blocking thread per connection.
    #[default]
    PerConnection,
    /// A fixed pool of worker threads shared by all connections.
    ThreadPool,
}

/// Which frame the buffer pool evicts to make room for a miss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EvictionPolicy {
    /// Least recently used. The historical default; keeps behaviour identical
    /// to the build before this key existed.
    #[default]
    Lru,
    /// Second-chance clock: a referenced frame survives one sweep.
    Clock,
    /// First in, first out: a hit does not change the order.
    Fifo,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub storage: StorageConfig,
    pub wal: WalConfig,
    pub server: ServerConfig,
    pub execution: ExecutionConfig,
    pub auth: AuthConfig,
    pub transaction: TransactionConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub default_engine: EngineKind,
    /// Page layout new heap tables use.
    pub page_layout: PageLayout,
    pub buffer_pool_frames: usize,
    pub double_write: bool,
    pub inline_lob_limit: usize,
    /// Live SSTable count that triggers an automatic LSM compaction.
    pub lsm_compaction_trigger: usize,
    /// Which frame the buffer pool evicts on a miss.
    pub eviction: EvictionPolicy,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WalConfig {
    pub checkpoint_threshold: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub addr: String,
    /// Optional second listener serving the HTTP/JSON frontend.
    pub http_addr: Option<String>,
    /// Optional third listener speaking the MySQL wire protocol.
    pub mysql_addr: Option<String>,
    /// Directory holding the built Vue SPA, served from the HTTP listener.
    /// Relative paths resolve against the process CWD, like `config.toml`.
    /// An empty string disables static hosting (the field can no longer say
    /// "off" by being absent, since it has a default).
    pub web_root: Option<String>,
    pub protocols: Vec<String>,
    pub thread_model: ThreadModel,
    pub worker_threads: usize,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionConfig {
    pub mode: ExecutionMode,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TransactionConfig {
    /// Isolation level for explicit transactions and autocommit statements; it
    /// determines how concurrent writes conflict (EPQ vs abort).
    pub isolation: Isolation,
    /// How long a writer waits for a row lock before giving up.
    pub lock_timeout_ms: u64,
}

impl Default for TransactionConfig {
    fn default() -> Self {
        Self { isolation: Isolation::ReadCommitted, lock_timeout_ms: 5000 }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            default_engine: EngineKind::Heap,
            page_layout: PageLayout::Row,
            buffer_pool_frames: 64,
            double_write: false,
            inline_lob_limit: 4096,
            lsm_compaction_trigger: 4,
            eviction: EvictionPolicy::Lru,
        }
    }
}

impl Default for WalConfig {
    fn default() -> Self {
        Self { checkpoint_threshold: 8 * 1024 * 1024 }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1:5678".into(),
            http_addr: None,
            mysql_addr: None,
            web_root: Some("web/dist".into()),
            protocols: vec!["text".into()],
            thread_model: ThreadModel::PerConnection,
            worker_threads: 4,
        }
    }
}

impl Config {
    /// Loads `config.toml`. A missing file is not an error: defaults apply.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let cfg = Self::from_toml_str(&text)?;
                cfg.validate()?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::Runtime(format!(
                "cannot read config {}: {e}",
                path.display()
            ))),
        }
    }

    /// Parses a TOML string; missing entries fall back to defaults.
    /// Callers must call [`Config::validate`] to reject impossible values.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| Error::Runtime(format!("invalid config: {e}")))
    }

    pub fn validate(&self) -> Result<()> {
        if self.storage.buffer_pool_frames == 0 {
            return Err(Error::Runtime(
                "storage.buffer_pool_frames must be greater than zero".into(),
            ));
        }
        if self.server.thread_model == ThreadModel::ThreadPool && self.server.worker_threads == 0 {
            return Err(Error::Runtime(
                "server.worker_threads must be greater than zero for the thread-pool model".into(),
            ));
        }
        Ok(())
    }
}
