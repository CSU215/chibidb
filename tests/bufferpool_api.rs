//! The pool panel's three endpoints: what is cached, what is in the cache, and
//! what just happened to it.
//!
//! These assert the contract the console polls -- cumulative counters (so a
//! rate is the reader's subtraction, not the engine's window), a frame table,
//! and an event log read from a cursor.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{mpsc, Arc};

use chibidb::config::Config;
use chibidb::instance::Instance;

fn start_server(config: Config) -> (std::net::SocketAddr, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let instance = Arc::new(Instance::open(dir.path(), &config).unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let _ = chibidb::http::serve(instance, listener).await;
        });
    });
    (rx.recv().unwrap(), dir)
}

fn get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn post(addr: std::net::SocketAddr, path: &str, body: &str) -> String {
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    request.push_str(body);
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn status(response: &str) -> String {
    response.lines().next().unwrap_or("").to_string()
}

fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}

/// The metrics payload, asserted to be a 200 first.
fn metrics(addr: std::net::SocketAddr) -> String {
    let response = get(addr, "/api/metrics");
    assert!(status(&response).contains("200"), "{response}");
    body(&response).to_string()
}

/// The value of a numeric field, read straight out of the JSON text.
fn number(payload: &str, key: &str) -> u64 {
    let needle = format!("\"{key}\":");
    let at = payload.find(&needle).unwrap_or_else(|| panic!("no {key} in {payload}"));
    let rest = &payload[at + needle.len()..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().unwrap_or_else(|_| panic!("{key} is not a number in {payload}"))
}

fn admin_config() -> Config {
    Config::from_toml_str("[server]\nadmin_api = true\n").unwrap()
}

fn setup(addr: std::net::SocketAddr, sql: &str) {
    let quoted = format!("{{\"sql\":\"{}\"}}", sql.replace('"', "\\\""));
    let response = post(addr, "/query", &quoted);
    assert!(status(&response).contains("200"), "{response}");
}

/// A heap table with an index, and an LSM table, all touched once so the pool
/// and the WAL have something to report.
fn seeded(addr: std::net::SocketAddr) {
    setup(addr, "create table t (id int, name text);");
    setup(addr, "create index idx_id on t (id);");
    setup(addr, "insert into t values (1,'a'),(2,'b'),(3,'c');");
    setup(addr, "select * from t where id = 2;");
    setup(addr, "create table l (id int, body text) engine = lsm;");
    setup(addr, "insert into l values (1,'x');");
}

#[test]
fn metrics_report_the_pool_and_the_wal() {
    let (addr, _dir) = start_server(admin_config());
    seeded(addr);

    let payload = metrics(addr);
    // Capacity comes from the config default; resident never exceeds it.
    assert_eq!(number(&payload, "capacity"), 64, "{payload}");
    assert!(number(&payload, "resident") <= 64, "{payload}");
    assert!(payload.contains("\"hit_rate\":"), "{payload}");
    // The seeded statements had to do I/O, so there are lookups to report.
    assert!(number(&payload, "hits") + number(&payload, "misses") > 0, "{payload}");
    // The WAL has the inserts in it, and the threshold is the config default.
    assert!(number(&payload, "bytes") > 0, "{payload}");
    assert_eq!(number(&payload, "threshold"), 8 * 1024 * 1024, "{payload}");
    // The LSM table reports its levels and memtable, the heap table does not.
    assert!(payload.contains("\"table\":\"l\""), "{payload}");
    assert!(payload.contains("\"levels\":["), "{payload}");
    assert!(number(&payload, "memtable_bytes") > 0, "{payload}");
    assert!(!payload.contains("\"table\":\"t\""), "{payload}");
}

#[test]
fn metrics_counters_are_cumulative() {
    let (addr, _dir) = start_server(admin_config());
    seeded(addr);

    let before = metrics(addr);
    let lookups = |payload: &str| number(payload, "hits") + number(payload, "misses");
    setup(addr, "select * from t;");
    let after = metrics(addr);
    assert!(
        lookups(&after) > lookups(&before),
        "累计值必须随查询推进：{before} -> {after}"
    );
    // The cumulative counters are what a reader subtracts to get a rate, so
    // they must never go backwards.
    assert!(number(&after, "evictions") >= number(&before, "evictions"), "{after}");
}

#[test]
fn the_frame_table_lists_what_is_resident() {
    let (addr, _dir) = start_server(admin_config());
    seeded(addr);

    let response = get(addr, "/api/bufferpool/frames");
    assert!(status(&response).contains("200"), "{response}");
    let payload = body(&response).to_string();
    assert!(payload.contains("\"frames\":[{"), "{payload}");
    assert!(payload.contains("\"pins\":"), "{payload}");
    assert!(payload.contains("\"dirty\":"), "{payload}");
    assert!(payload.contains("\"accessed\":"), "{payload}");

    // The snapshot agrees with the summary the panel shows next to it.
    let metrics = metrics(addr);
    let frames = payload.matches("\"page\":").count() as u64;
    assert_eq!(frames, number(&metrics, "resident"), "{payload} / {metrics}");
}

#[test]
fn the_event_log_resumes_from_a_cursor() {
    let (addr, _dir) = start_server(admin_config());
    seeded(addr);

    let response = get(addr, "/api/bufferpool/events?since=0");
    assert!(status(&response).contains("200"), "{response}");
    let first = body(&response).to_string();
    assert!(first.contains("\"kind\":\"load\""), "载入应当留痕：{first}");
    assert!(first.contains("\"seq\":1,"), "序号从 1 开始：{first}");
    assert!(first.contains("\"truncated\":false"), "{first}");
    let next = number(&first, "next");
    assert!(next > 0, "{first}");

    // Resuming from the cursor: nothing new, and the cursor does not move.
    let idle = body(&get(addr, &format!("/api/bufferpool/events?since={next}"))).to_string();
    assert!(idle.contains("\"events\":[]"), "{idle}");
    assert_eq!(number(&idle, "next"), next, "{idle}");

    // A missing cursor means "from the beginning".
    let again = body(&get(addr, "/api/bufferpool/events")).to_string();
    assert_eq!(number(&again, "next"), next, "{again}");
}

#[test]
fn the_pool_endpoints_are_part_of_the_admin_surface() {
    let (addr, _dir) = start_server(Config::default());
    for path in ["/api/metrics", "/api/bufferpool/frames", "/api/bufferpool/events"] {
        let response = get(addr, path);
        assert!(status(&response).contains("404"), "{path}: {response}");
    }
}
