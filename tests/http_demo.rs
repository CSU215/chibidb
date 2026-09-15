//! The demo console's backing API: `/api/plan`, `/api/config`, `/api/schema`,
//! `/api/files`, `/api/page` and the embedded static assets.
//!
//! These tests start a real HTTP listener for an in-memory instance, exactly as
//! `http_admin.rs` does, so they exercise the whole request path.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{mpsc, Arc};

use chaoticdb::config::Config;
use chaoticdb::instance::Instance;

/// Starts the HTTP frontend on an ephemeral port and returns its address plus
/// the data directory (kept alive for the test).
fn start_server(config: Config) -> (std::net::SocketAddr, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let instance = Arc::new(Instance::open(dir.path(), &config).unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let _ = chaoticdb::http::serve(instance, listener).await;
        });
    });
    (rx.recv().unwrap(), dir)
}

/// The demo console switched on, with the admin API and (optionally) the raw
/// disk preview.
fn demo_config(page_preview: bool) -> Config {
    Config::from_toml_str(&format!(
        "[server]\nadmin_api = true\n[web]\nenabled = true\ntitle = \"chaoticdb console\"\npage_preview = {page_preview}\n"
    ))
    .unwrap()
}

/// Runs one statement and returns the raw response.
fn query(addr: std::net::SocketAddr, sql: &str) -> String {
    let body = format!("{{\"sql\":\"{sql}\"}}");
    let request = format!(
        "POST /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    send(addr, &request)
}

/// Posts a raw JSON body to a path.
fn post(addr: std::net::SocketAddr, path: &str, body: &str) -> String {
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    send(addr, &request)
}

/// Fetches a path.
fn get(addr: std::net::SocketAddr, path: &str) -> String {
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    send(addr, &request)
}

fn send(addr: std::net::SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

/// The response body, after the header block.
fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}

/// Creates a table with an index and one row, then checkpoints so the raw disk
/// preview has something to show: unflushed buffer-pool pages are not readable.
fn setup(addr: std::net::SocketAddr) {
    for sql in [
        "create table t (id int primary key, name char(20));",
        "insert into t values (1, 'alice');",
        "checkpoint;",
    ] {
        let response = query(addr, sql);
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{sql}: {response}");
    }
}

/// A number following `"key":`.
fn number_field(payload: &str, key: &str) -> Option<i64> {
    let needle = format!("\"{key}\":");
    let rest = &payload[payload.find(&needle)? + needle.len()..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[test]
fn plan_reports_an_index_scan_for_an_indexed_predicate() {
    let (addr, _dir) = start_server(demo_config(true));
    setup(addr);

    let response = post(addr, "/api/plan", r#"{"sql":"select * from t where id = 1;"}"#);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""tokens":[{"kind":"#), "{payload}");
    assert!(payload.contains(r#""statements":[{"debug":"#), "{payload}");
    assert!(!payload.contains(r#""physical":null"#), "{payload}");
    assert!(payload.contains("IndexScan"), "{payload}");
    assert!(!payload.contains(r#""plan":null"#), "{payload}");
    assert!(payload.contains(r#""error":null"#), "{payload}");
}

#[test]
fn plan_on_a_fresh_instance_shows_tokens_without_an_error() {
    let (addr, _dir) = start_server(demo_config(true));

    // No table, and no database has been created yet. The token stream and AST
    // are still worth showing; the missing database is not an error.
    let response = post(addr, "/api/plan", r#"{"sql":"select 1;"}"#);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""error":null"#), "{payload}");
    assert!(payload.contains(r#""physical":null"#), "{payload}");
    assert!(payload.contains(r#""tokens":[{"kind":"#), "{payload}");
}

#[test]
fn plan_reports_a_plan_stage_error_for_a_missing_table() {
    let (addr, _dir) = start_server(demo_config(true));
    setup(addr);

    let response = post(addr, "/api/plan", r#"{"sql":"select * from nope;"}"#);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""stage":"plan""#), "{payload}");
    assert!(payload.contains(r#""pos":null"#), "{payload}");
}

#[test]
fn config_reports_title_and_page_preview() {
    let (addr, _dir) = start_server(demo_config(true));

    let response = get(addr, "/api/config");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""title":"chaoticdb console""#), "{payload}");
    assert!(payload.contains(r#""page_preview":true"#), "{payload}");
    assert!(payload.contains(r#""admin_api":true"#), "{payload}");
}

#[test]
fn buffer_reports_a_pool_per_database() {
    // Not gated by `web.page_preview`: the counters expose no raw page contents.
    let (addr, _dir) = start_server(demo_config(false));
    setup(addr);

    let response = get(addr, "/api/buffer");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""frame_size":8192"#), "{payload}");
    assert!(payload.contains(r#""eviction":"lru""#), "{payload}");
    assert!(payload.contains(r#""name":"chibi_meta""#), "{payload}");
    assert!(payload.contains(r#""name":"main""#), "{payload}");
    assert!(payload.contains(r#""capacity":64"#), "{payload}");
    assert!(payload.contains(r#""hit_rate":"#), "{payload}");
    assert!(payload.contains(r#""total":{"#), "{payload}");
}

#[test]
fn schema_lists_the_created_table_and_columns() {
    let (addr, _dir) = start_server(demo_config(true));
    setup(addr);

    let response = get(addr, "/api/schema");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""name":"main""#), "{payload}");
    assert!(payload.contains(r#""name":"t""#), "{payload}");
    assert!(payload.contains(r#""name":"id""#), "{payload}");
    assert!(payload.contains(r#""name":"name""#), "{payload}");
    assert!(payload.contains(r#""type":"int""#), "{payload}");
    assert!(payload.contains(r#""primary_key":true"#), "{payload}");
}

#[test]
fn files_lists_the_catalog_and_the_table_file() {
    let (addr, _dir) = start_server(demo_config(true));
    setup(addr);

    let response = get(addr, "/api/files?db=main");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""name":"catalog.bin""#), "{payload}");
    assert!(payload.contains(r#""name":"tables/000000.dbf""#), "{payload}");
    assert!(payload.contains(r#""kind":"heap""#), "{payload}");
}

#[test]
fn page_zero_is_a_file_header_and_page_one_is_slotted() {
    let (addr, _dir) = start_server(demo_config(true));
    setup(addr);

    let header = get(addr, "/api/page?db=main&file=tables%2F000000.dbf&no=0");
    assert!(header.starts_with("HTTP/1.1 200 OK"), "{header}");
    let payload = body(&header);
    assert!(payload.contains(r#""structure":{"type":"file_header""#), "{payload}");
    assert!(!payload.contains(r#""hex":"""#), "the page must have bytes: {payload}");

    let data = get(addr, "/api/page?db=main&file=tables%2F000000.dbf&no=1");
    assert!(data.starts_with("HTTP/1.1 200 OK"), "{data}");
    let payload = body(&data);
    assert!(payload.contains(r#""type":"slotted""#), "{payload}");
    let slots = number_field(payload, "num_slots").expect("num_slots must be a number");
    assert!(slots >= 1, "expected at least one slot: {payload}");
}

#[test]
fn root_serves_the_embedded_console() {
    let (addr, _dir) = start_server(demo_config(true));

    let response = get(addr, "/");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(
        response.to_ascii_lowercase().contains("content-type: text/html"),
        "{response}"
    );
    assert!(body(&response).contains("chaoticdb"), "{response}");
}

#[test]
fn files_is_404_when_page_preview_is_off() {
    let (addr, _dir) = start_server(demo_config(false));
    setup(addr);

    let response = get(addr, "/api/files?db=main");
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
}

#[test]
fn page_is_404_when_page_preview_is_off() {
    let (addr, _dir) = start_server(demo_config(false));
    setup(addr);

    let response = get(addr, "/api/page?db=main&file=tables%2F000000.dbf&no=0");
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
}

/// Enabling the built-in console is enough; `server.admin_api` is not required
/// for the plan/schema/config endpoints. Raw page previews still need
/// `web.page_preview`.
#[test]
fn the_demo_console_enables_its_own_api() {
    let config = Config::from_toml_str(
        "[server]\nadmin_api = false\n[web]\nenabled = true\npage_preview = false\n",
    )
    .unwrap();
    let (addr, _dir) = start_server(config);
    setup(addr);

    for path in ["/api/config", "/api/schema", "/api/buffer"] {
        let response = get(addr, path);
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{path}: {response}");
    }
    let response = post(addr, "/api/plan", r#"{"sql":"select 1;"}"#);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");

    // The raw disk preview is a separate switch and stays off.
    let files = get(addr, "/api/files?db=main");
    assert!(files.starts_with("HTTP/1.1 404"), "{files}");
}

/// With neither switch on, the whole namespace is closed.
#[test]
fn the_api_is_404_without_admin_api_or_the_console() {
    let config = Config::from_toml_str("[server]\nadmin_api = false\n[web]\nenabled = false\n").unwrap();
    let (addr, _dir) = start_server(config);

    for path in ["/api/config", "/api/schema", "/api/buffer"] {
        let response = get(addr, path);
        assert!(response.starts_with("HTTP/1.1 404"), "{path}: {response}");
    }
    let response = post(addr, "/api/plan", r#"{"sql":"select 1;"}"#);
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
}
