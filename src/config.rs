use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Error, Result};

/// Which storage engine new tables use. Existing files record their own engine,
/// so this only affects creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    #[default]
    Heap,
    Lsm,
}

/// Execution model: row-at-a-time (Volcano) or columnar batches (Chunk).
/// Chunk is the default; Volcano stays selectable and is the reference the
/// differential tests compare against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    Volcano,
    #[default]
    Chunk,
}

/// Physical page layout for heap tables. `Pax` stores a page column-major so a
/// scan reads only the columns it needs; `Row` is the slotted record layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PageLayout {
    #[default]
    Row,
    Pax,
}

/// Transaction isolation, following PostgreSQL's model. The difference is only
/// *when* a snapshot is taken and how a write-write conflict is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// Each statement sees the latest committed data; a writer whose row was
    /// concurrently updated restarts the statement with a fresh snapshot (EPQ).
    #[default]
    ReadCommitted,
    /// One stable snapshot for the whole transaction; a conflicting writer gets
    /// a serialization failure.
    RepeatableRead,
    /// Repeatable read plus SSI: a transaction closing a cycle of read/write
    /// dependencies is aborted.
    Serializable,
}

/// How accepted connections are mapped to execution threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ThreadModel {
    #[default]
    PerConnection,
    ThreadPool,
}

/// Which frame the buffer pool evicts to make room for a miss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EvictionPolicy {
    #[default]
    Lru,
    Clock,
    Fifo,
}

/// What `server.web_root` resolves to. The startup note and the fallback page
/// key off this, so the two never disagree about why nothing is served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebRootState {
    Off,
    Missing(String),
    Ready(PathBuf),
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
    pub observability: ObservabilityConfig,
    pub web: WebConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub default_engine: EngineKind,
    pub page_layout: PageLayout,
    pub buffer_pool_frames: usize,
    pub double_write: bool,
    pub inline_lob_limit: usize,
    pub lsm_compaction_trigger: usize,
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
    pub http_addr: Option<String>,
    pub mysql_addr: Option<String>,
    pub web_root: Option<String>,
    pub admin_api: bool,
    pub protocols: Vec<String>,
    pub thread_model: ThreadModel,
    pub worker_threads: usize,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionConfig {
    pub mode: ExecutionMode,
}

/// The built-in, no-build demo console served straight from the binary.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    /// Serve the built-in, no-build demo console at `/` from the binary.
    pub enabled: bool,
    /// Console title shown in the header.
    pub title: String,
    /// Allow the read-only disk preview endpoints (`/api/files`, `/api/page`).
    /// Off by default: they expose raw data pages.
    pub page_preview: bool,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            title: "chaoticdb console".into(),
            page_preview: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObservabilityConfig {
    pub cache_stats: bool,
    pub eviction_log: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TransactionConfig {
    pub isolation: Isolation,
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
            admin_api: false,
            protocols: vec!["text".into()],
            thread_model: ThreadModel::PerConnection,
            worker_threads: 4,
        }
    }
}

impl Config {
    /// Loads `config.toml`; a missing file is not an error (defaults apply).
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

    /// Parses a TOML string; missing entries fall back to defaults. Callers
    /// must call [`Config::validate`] to reject impossible values.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| Error::Runtime(format!("invalid config: {e}")))
    }

    /// Resolves `server.web_root` against the process CWD, the way
    /// `config.toml` itself is resolved.
    pub fn web_root_state(&self) -> WebRootState {
        let Some(configured) = self.server.web_root.as_deref().filter(|r| !r.is_empty()) else {
            return WebRootState::Off;
        };
        match Path::new(configured).canonicalize() {
            Ok(path) if path.is_dir() => WebRootState::Ready(path),
            _ => WebRootState::Missing(configured.to_string()),
        }
    }

    /// Whether a built web console exists but has nowhere to be served from,
    /// because no HTTP listener is configured.
    ///
    /// This is a silent failure otherwise: `serve` starts happily, prints one
    /// text-protocol line, and the console answers nothing -- the browser (or
    /// the dev server's proxy) reports a 5xx that names neither the cause nor
    /// the fix. Hence the check. Deliberately false when there is no built
    /// console, so a backend-only setup stays quiet.
    pub fn web_console_unreachable(&self) -> bool {
        self.server.http_addr.is_none() && matches!(self.web_root_state(), WebRootState::Ready(_))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_usable() {
        let c = Config::default();
        assert_eq!(c.storage.default_engine, EngineKind::Heap);
        assert_eq!(c.execution.mode, ExecutionMode::Chunk);
        assert_eq!(c.transaction.isolation, Isolation::ReadCommitted);
        c.validate().unwrap();
    }

    #[test]
    fn toml_overrides_only_the_named_keys() {
        let c = Config::from_toml_str(
            "[storage]\nbuffer_pool_frames = 8\n[execution]\nmode = \"chunk\"\n",
        )
        .unwrap();
        assert_eq!(c.storage.buffer_pool_frames, 8);
        assert_eq!(c.execution.mode, ExecutionMode::Chunk);
        assert_eq!(c.storage.inline_lob_limit, 4096, "unnamed keys keep defaults");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(Config::from_toml_str("[storage]\nnope = 1\n").is_err());
    }

    #[test]
    fn web_section_parses() {
        let c = Config::from_toml_str(
            "[web]\nenabled = true\ntitle = \"x\"\npage_preview = true\n",
        )
        .unwrap();
        assert!(c.web.enabled);
        assert_eq!(c.web.title, "x");
        assert!(c.web.page_preview);

        let d = Config::default().web;
        assert!(!d.enabled);
        assert_eq!(d.title, "chaoticdb console");
        assert!(!d.page_preview);
    }

    #[test]
    fn zero_frames_is_invalid() {
        let c = Config::from_toml_str("[storage]\nbuffer_pool_frames = 0\n").unwrap();
        assert!(c.validate().is_err());
    }
}
