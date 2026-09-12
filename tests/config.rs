use std::io::Write;

use chibidb::config::{Config, ConflictStrategy, EngineKind, ExecutionMode, ThreadModel};
use chibidb::value::Value;
use chibidb::{Database, ResultSet};

#[test]
fn defaults_are_sane() {
    let c = Config::default();
    assert_eq!(c.storage.buffer_pool_frames, 64);
    assert_eq!(c.storage.default_engine, EngineKind::Heap);
    assert!(!c.storage.double_write);
    assert_eq!(c.storage.inline_lob_limit, 4096);
    assert_eq!(c.wal.checkpoint_threshold, 8 * 1024 * 1024);
    assert_eq!(c.server.addr, "127.0.0.1:5678");
    assert_eq!(c.server.protocols, ["text"]);
    assert_eq!(c.execution.mode, ExecutionMode::Volcano);
    assert!(!c.auth.enabled);
    assert_eq!(c.transaction.conflict, ConflictStrategy::Fcw);
}

#[test]
fn transaction_conflict_strategy_parses() {
    let c = Config::from_toml_str("[transaction]\nconflict = \"2pl\"\n").unwrap();
    assert_eq!(c.transaction.conflict, ConflictStrategy::TwoPl);
    let c = Config::from_toml_str("[transaction]\nconflict = \"fcw\"\n").unwrap();
    assert_eq!(c.transaction.conflict, ConflictStrategy::Fcw);
}

#[test]
fn partial_toml_fills_remaining_defaults() {
    let c = Config::from_toml_str("[storage]\ninline_lob_limit = 2048\n").unwrap();
    assert_eq!(c.storage.inline_lob_limit, 2048);
    assert_eq!(c.storage.buffer_pool_frames, 64);
    assert_eq!(c.storage.default_engine, EngineKind::Heap);
    assert_eq!(c.server.addr, "127.0.0.1:5678");
}

#[test]
fn parses_all_sections() {
    let toml = r#"
[storage]
default_engine = "lsm"
buffer_pool_frames = 128
double_write = true
inline_lob_limit = 2048

[wal]
checkpoint_threshold = 1048576

[server]
addr = "0.0.0.0:4000"
protocols = ["text", "mysql"]

[execution]
mode = "chunk"

[auth]
enabled = true
"#;
    let c = Config::from_toml_str(toml).unwrap();
    assert_eq!(c.storage.default_engine, EngineKind::Lsm);
    assert_eq!(c.storage.buffer_pool_frames, 128);
    assert!(c.storage.double_write);
    assert_eq!(c.storage.inline_lob_limit, 2048);
    assert_eq!(c.wal.checkpoint_threshold, 1048576);
    assert_eq!(c.server.addr, "0.0.0.0:4000");
    assert_eq!(c.server.protocols, ["text", "mysql"]);
    assert_eq!(c.execution.mode, ExecutionMode::Chunk);
    assert!(c.auth.enabled);
}

#[test]
fn rejects_unknown_field() {
    assert!(Config::from_toml_str("[storage]\nbogus = 1\n").is_err());
}

#[test]
fn rejects_page_size_as_unknown_field() {
    // page size is a compile-time constant; a config must not try to set it.
    assert!(Config::from_toml_str("[storage]\npage_size = 8192\n").is_err());
}

#[test]
fn rejects_zero_buffer_pool_frames() {
    let c = Config::from_toml_str("[storage]\nbuffer_pool_frames = 0\n").unwrap();
    assert!(c.validate().is_err());
}

#[test]
fn parses_thread_model() {
    let per = Config::from_toml_str("[server]\nthread_model = \"per-connection\"\n").unwrap();
    assert_eq!(per.server.thread_model, ThreadModel::PerConnection);

    let pool =
        Config::from_toml_str("[server]\nthread_model = \"thread-pool\"\nworker_threads = 8\n")
            .unwrap();
    assert_eq!(pool.server.thread_model, ThreadModel::ThreadPool);
    assert_eq!(pool.server.worker_threads, 8);

    let zero = Config::from_toml_str(
        "[server]\nthread_model = \"thread-pool\"\nworker_threads = 0\n",
    )
    .unwrap();
    assert!(zero.validate().is_err());
}

#[test]
fn missing_file_yields_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let c = Config::load(&dir.path().join("nope.toml")).unwrap();
    assert_eq!(c, Config::default());
}

#[test]
fn load_reads_and_validates_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "[storage]\nbuffer_pool_frames = 8").unwrap();
    let c = Config::load(&path).unwrap();
    assert_eq!(c.storage.buffer_pool_frames, 8);
}

#[test]
fn load_rejects_invalid_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[storage]\nbuffer_pool_frames = 0\n").unwrap();
    assert!(Config::load(&path).is_err());
}

#[test]
fn database_applies_config_buffer_pool_frames() {
    let cfg = Config::from_toml_str("[storage]\nbuffer_pool_frames = 2\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with_config(dir.path(), &cfg).unwrap();
    assert_eq!(db.config().storage.buffer_pool_frames, 2);

    db.execute_sql("create table t (id int);").unwrap();
    for i in 0..50 {
        db.execute_sql(&format!("insert into t values ({i});")).unwrap();
    }
    let rs = db.execute_sql("select count(*) from t;").unwrap();
    match &rs[0] {
        ResultSet::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(50)),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn open_in_memory_honors_config() {
    let cfg = Config::from_toml_str("[storage]\nbuffer_pool_frames = 1\n").unwrap();
    let db = Database::open_in_memory_with_config(&cfg).unwrap();
    assert_eq!(db.config().storage.buffer_pool_frames, 1);
}

#[test]
fn example_config_stays_valid() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let cfg = Config::from_toml_str(&text).unwrap();
    cfg.validate().unwrap();
    assert_eq!(cfg, Config::default());
}
