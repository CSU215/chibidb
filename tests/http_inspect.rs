//! The disk-inspection API: `/api/schema`, `/api/files`, `/api/overview` and
//! `/api/page` over real on-disk structures, including the system database.
//!
//! These tests start a real HTTP listener for an in-memory instance, exactly as
//! `http_demo.rs` does, so they exercise the whole request path.

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

/// The demo console switched on with the raw disk preview.
fn demo_config(page_preview: bool) -> Config {
    Config::from_toml_str(&format!(
        "[server]\nadmin_api = true\n[web]\nenabled = true\npage_preview = {page_preview}\n"
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

/// Runs each statement, asserting the request succeeded.
fn run(addr: std::net::SocketAddr, statements: &[&str]) {
    for sql in statements {
        let response = query(addr, sql);
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{sql}: {response}");
    }
}

/// Percent-encodes `/` so a relative path can travel in a query string.
fn encode_file(file: &str) -> String {
    file.replace('/', "%2F")
}

/// The first `"name":"..."` value containing `needle`.
fn file_named(payload: &str, needle: &str) -> Option<String> {
    let marker = "\"name\":\"";
    let mut rest = payload;
    while let Some(at) = rest.find(marker) {
        let start = at + marker.len();
        let end = start + rest[start..].find('"')?;
        let name = &rest[start..end];
        if name.contains(needle) {
            return Some(name.to_string());
        }
        rest = &rest[end..];
    }
    None
}

// --------------------------------------------------------------------- schema

#[test]
fn schema_lists_the_system_database_first() {
    let (addr, _dir) = start_server(demo_config(true));
    // Create the default database so both it and the system one are listed.
    run(addr, &["create table t (id int);"]);

    let response = get(addr, "/api/schema");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""name":"chibi_meta","system":true"#), "{payload}");
    assert!(payload.contains(r#""name":"main","system":false"#), "{payload}");
    let system_at = payload.find(r#""name":"chibi_meta""#).unwrap();
    let main_at = payload.find(r#""name":"main""#).unwrap();
    assert!(system_at < main_at, "the system database must come first: {payload}");
}

// ---------------------------------------------------------------------- files

#[test]
fn files_reaches_the_system_database() {
    let (addr, _dir) = start_server(demo_config(true));

    let response = get(addr, "/api/files?db=chibi_meta");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""db":"chibi_meta""#), "{payload}");
    assert!(payload.contains(r#""system":true"#), "{payload}");
    assert!(payload.contains(r#""name":"catalog.bin""#), "{payload}");
    assert!(payload.contains(r#""name":"tables/000000.dbf""#), "{payload}");
}

#[test]
fn files_lists_lsm_tables_as_manifest_and_sstable() {
    let (addr, _dir) = start_server(demo_config(true));
    run(
        addr,
        &[
            "create table l (id int primary key, v int) engine = lsm;",
            "insert into l values (1, 10);",
            "checkpoint;",
        ],
    );

    let response = get(addr, "/api/files?db=main");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains("tables/000000.lsm/MANIFEST"), "{payload}");
    assert!(payload.contains(r#""kind":"lsm_manifest""#), "{payload}");
    assert!(payload.contains("tables/000000.lsm/sst-000001.sst"), "{payload}");
    assert!(payload.contains(r#""kind":"lsm_sstable""#), "{payload}");
    assert!(payload.contains(r#""unit_kind":"region""#), "{payload}");
}

// ------------------------------------------------------------------- overview

#[test]
fn overview_of_a_heap_file_marks_header_and_slotted_pages() {
    let (addr, _dir) = start_server(demo_config(true));
    run(
        addr,
        &[
            "create table t (id int primary key, name char(20));",
            "insert into t values (1, 'alice');",
            "insert into t values (2, 'bob');",
            "checkpoint;",
        ],
    );

    let response = get(addr, "/api/overview?db=main&file=tables%2F000000.dbf");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""mode":"heap-row""#), "{payload}");
    assert!(payload.contains(r#""unit_kind":"page""#), "{payload}");
    assert!(
        payload.contains(r#"{"index":0,"label":"0","kind":"file_header","used":true"#),
        "{payload}"
    );
    assert!(
        payload.contains(r#"{"index":1,"label":"1","kind":"slotted","used":true"#),
        "{payload}"
    );
}

#[test]
fn overview_is_404_when_page_preview_is_off() {
    let (addr, _dir) = start_server(demo_config(false));
    run(addr, &["create table t (id int);"]);

    for path in [
        "/api/overview?db=main&file=tables%2F000000.dbf",
        "/api/files?db=main",
    ] {
        let response = get(addr, path);
        assert!(response.starts_with("HTTP/1.1 404"), "{path}: {response}");
    }
}

// ----------------------------------------------------------------------- page

#[test]
fn heap_page_zero_has_a_magic_field_and_the_row_mode() {
    let (addr, _dir) = start_server(demo_config(true));
    run(
        addr,
        &[
            "create table t (id int primary key, name char(20));",
            "insert into t values (1, 'alice');",
            "checkpoint;",
        ],
    );

    let response = get(addr, "/api/page?db=main&file=tables%2F000000.dbf&no=0");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""mode":"file-header""#), "{payload}");
    assert!(payload.contains(r#""label":"magic","start":0,"len":8"#), "{payload}");
    assert!(!payload.contains(r#""hex":"""#), "the page must have bytes: {payload}");
}

#[test]
fn heap_data_page_spans_slots_and_records() {
    let (addr, _dir) = start_server(demo_config(true));
    run(
        addr,
        &[
            "create table t (id int primary key, name char(20));",
            "insert into t values (1, 'alice');",
            "checkpoint;",
        ],
    );

    let response = get(addr, "/api/page?db=main&file=tables%2F000000.dbf&no=1");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""mode":"heap-row""#), "{payload}");
    assert!(payload.contains(r#""label":"num_slots""#), "{payload}");
    assert!(payload.contains(r#""kind":"slot"#), "{payload}");
    assert!(payload.contains(r#""label":"slot_directory""#), "{payload}");
    assert!(
        payload.contains(r#""kind":"record""#) || payload.contains(r#""kind":"slot""#),
        "{payload}"
    );
}

#[test]
fn pax_table_uses_the_pax_mode_and_magic() {
    let (addr, _dir) = start_server(demo_config(true));
    run(
        addr,
        &[
            "create table p (id int primary key, name char(20)) page_layout = pax;",
            "insert into p values (1, 'alice');",
            "checkpoint;",
        ],
    );

    let response = get(addr, "/api/page?db=main&file=tables%2F000000.dbf&no=1");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""mode":"heap-pax""#), "{payload}");
    assert!(payload.contains(r#""label":"magic","start":0,"len":4"#), "{payload}");
}

#[test]
fn index_files_use_the_btree_mode() {
    let (addr, _dir) = start_server(demo_config(true));
    run(
        addr,
        &[
            "create table t (id int, name char(20));",
            "create index idx on t (id);",
            "insert into t values (1, 'alice');",
            "checkpoint;",
        ],
    );

    let overview = get(addr, "/api/overview?db=main&file=indexes%2F000000.idxf");
    assert!(overview.starts_with("HTTP/1.1 200 OK"), "{overview}");
    assert!(body(&overview).contains(r#""mode":"btree""#), "{overview}");

    let page = get(addr, "/api/page?db=main&file=indexes%2F000000.idxf&no=1");
    assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
    let payload = body(&page);
    assert!(payload.contains(r#""mode":"btree""#), "{payload}");
    assert!(payload.contains(r#""label":"node_type""#), "{payload}");
    assert!(payload.contains(r#""label":"entries""#), "{payload}");
}

#[test]
fn wal_frames_are_inspectable() {
    let (addr, _dir) = start_server(demo_config(true));
    // No checkpoint, so the committed write stays in the log.
    run(
        addr,
        &[
            "create table t (id int primary key, name char(20));",
            "insert into t values (1, 'alice');",
        ],
    );

    let response = get(addr, "/api/page?db=main&file=wal.bin&no=0");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""mode":"wal""#), "{payload}");
    assert!(payload.contains(r#""unit_kind":"frame""#), "{payload}");
    assert!(payload.contains(r#""label":"frame_len""#), "{payload}");
    assert!(payload.contains(r#""label":"frame_type""#), "{payload}");
}

#[test]
fn sstable_files_use_the_lsm_mode_and_expose_the_footer() {
    let (addr, _dir) = start_server(demo_config(true));
    run(
        addr,
        &[
            "create table l (id int primary key, v int) engine = lsm;",
            "insert into l values (1, 10);",
            "checkpoint;",
        ],
    );

    let files = get(addr, "/api/files?db=main");
    assert!(files.starts_with("HTTP/1.1 200 OK"), "{files}");
    let sst = file_named(body(&files), "sst-").expect("an sstable must be listed");
    let encoded = encode_file(&sst);

    // Page 0 is the first data block region.
    let data = get(addr, &format!("/api/page?db=main&file={encoded}&no=0"));
    assert!(data.starts_with("HTTP/1.1 200 OK"), "{data}");
    assert!(body(&data).contains(r#""mode":"lsm-sstable""#), "{}", body(&data));
    assert!(body(&data).contains(r#""kind":"lsm_sstable""#), "{}", body(&data));
    assert!(body(&data).contains(r#""type":"sstable_data_block""#), "{}", body(&data));

    // The footer is a distinct region; find its unit index from the overview.
    let overview = get(addr, &format!("/api/overview?db=main&file={encoded}"));
    assert!(overview.starts_with("HTTP/1.1 200 OK"), "{overview}");
    let footer_no = unit_index_with_kind(body(&overview), "footer").expect("a footer region");
    let response =
        get(addr, &format!("/api/page?db=main&file={encoded}&no={footer_no}"));
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""type":"sstable_footer""#), "{payload}");
    assert!(payload.contains(r#""label":"filter_offset""#), "{payload}");
    assert!(payload.contains(r#""label":"magic""#), "{payload}");
}

/// The `index` of the first overview unit whose `kind` is `kind`.
fn unit_index_with_kind(payload: &str, kind: &str) -> Option<u32> {
    let needle = format!("\"kind\":\"{kind}\"");
    let at = payload.find(&needle)?;
    let before = &payload[..at];
    let key = "\"index\":";
    let idx_at = before.rfind(key)? + key.len();
    let rest = &before[idx_at..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}
