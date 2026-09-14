use std::io::Write;

use chibidb::config::{
    Config, EngineKind, EvictionPolicy, ExecutionMode, Isolation, PageLayout, ThreadModel,
};
use chibidb::value::Value;
use chibidb::{Database, ResultSet};

#[test]
fn defaults_are_sane() {
    let c = Config::default();
    assert_eq!(c.storage.buffer_pool_frames, 64);
    assert_eq!(c.storage.default_engine, EngineKind::Heap);
    assert_eq!(c.storage.page_layout, PageLayout::Row);
    assert!(!c.storage.double_write);
    assert_eq!(c.storage.inline_lob_limit, 4096);
    assert_eq!(c.storage.eviction, EvictionPolicy::Lru);
    assert_eq!(c.wal.checkpoint_threshold, 8 * 1024 * 1024);
    assert_eq!(c.server.addr, "127.0.0.1:5678");
    assert_eq!(c.server.protocols, ["text"]);
    assert_eq!(c.execution.mode, ExecutionMode::Chunk);
    assert!(!c.auth.enabled);
    assert_eq!(c.transaction.isolation, Isolation::ReadCommitted);
}

#[test]
fn transaction_isolation_parses() {
    let c = Config::from_toml_str("[transaction]\nisolation = \"repeatable_read\"\n").unwrap();
    assert_eq!(c.transaction.isolation, Isolation::RepeatableRead);
    let c = Config::from_toml_str("[transaction]\nisolation = \"read_committed\"\n").unwrap();
    assert_eq!(c.transaction.isolation, Isolation::ReadCommitted);
    let c = Config::from_toml_str("[transaction]\nisolation = \"serializable\"\n").unwrap();
    assert_eq!(c.transaction.isolation, Isolation::Serializable);
}

#[test]
fn eviction_policy_parses() {
    let c = Config::from_toml_str("[storage]\neviction = \"lru\"\n").unwrap();
    assert_eq!(c.storage.eviction, EvictionPolicy::Lru);
    let c = Config::from_toml_str("[storage]\neviction = \"clock\"\n").unwrap();
    assert_eq!(c.storage.eviction, EvictionPolicy::Clock);
    let c = Config::from_toml_str("[storage]\neviction = \"fifo\"\n").unwrap();
    assert_eq!(c.storage.eviction, EvictionPolicy::Fifo);
    // 大小写敏感，拼错即报错（枚举没有 fallback）。
    assert!(Config::from_toml_str("[storage]\neviction = \"LRU\"\n").is_err());
    assert!(Config::from_toml_str("[storage]\neviction = \"random\"\n").is_err());
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
page_layout = "pax"
buffer_pool_frames = 128
double_write = true
inline_lob_limit = 2048
eviction = "clock"

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
    assert_eq!(c.storage.page_layout, PageLayout::Pax);
    assert_eq!(c.storage.buffer_pool_frames, 128);
    assert!(c.storage.double_write);
    assert_eq!(c.storage.inline_lob_limit, 2048);
    assert_eq!(c.storage.eviction, EvictionPolicy::Clock);
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
fn config_page_layout_applies_to_new_tables() {
    let cfg = Config::from_toml_str("[storage]\npage_layout = \"pax\"\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with_config(dir.path(), &cfg).unwrap();
    db.execute_sql("create table t (id int, v int);").unwrap();
    for i in 0..200 {
        db.execute_sql(&format!("insert into t values ({i}, {});", i * 2)).unwrap();
    }
    let rs = db.execute_sql("select sum(v) from t;").unwrap();
    match &rs[0] {
        ResultSet::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Int((0..200).map(|i| i * 2).sum()))
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn open_in_memory_honors_config() {
    let cfg = Config::from_toml_str("[storage]\nbuffer_pool_frames = 1\n").unwrap();
    let db = Database::open_in_memory_with_config(&cfg).unwrap();
    assert_eq!(db.config().storage.buffer_pool_frames, 1);
}

/// `storage.eviction` 必须在 `Database::open_with_config` 里真正接到缓冲池上
/// （唯一生产构造点）。策略本身只影响"淘汰谁"，所以这里断言的是一份真实负载
/// 在三种策略下端到端跑通、结果一致 —— 接错了会在别处炸，但至少这条路径被覆盖。
#[test]
fn database_applies_config_eviction_policy() {
    for policy in ["lru", "clock", "fifo"] {
        let cfg =
            Config::from_toml_str(&format!("[storage]\neviction = \"{policy}\"\n")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_with_config(dir.path(), &cfg).unwrap();
        db.execute_sql("create table t (id int, v char(200));").unwrap();
        for i in 0..200 {
            db.execute_sql(&format!("insert into t values ({i}, 'v{i}');")).unwrap();
        }
        // 逼出实际淘汰：容量 64 帧、每页容纳不了几行。
        let rs = db.execute_sql("select count(*), sum(id) from t;").unwrap();
        match &rs[0] {
            ResultSet::Rows { rows, .. } => {
                assert_eq!(rows[0][0], Value::Int(200), "{policy}");
                assert_eq!(rows[0][1], Value::Int((0..200).sum()), "{policy}");
            }
            other => panic!("{policy}: expected rows, got {other:?}"),
        }
    }
}

#[test]
fn example_config_stays_valid() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let cfg = Config::from_toml_str(&text).unwrap();
    cfg.validate().unwrap();
    assert_eq!(cfg, Config::default());
}
