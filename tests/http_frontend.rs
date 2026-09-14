use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{mpsc, Arc};

use chaoticdb::config::Config;
use chaoticdb::instance::Instance;

/// Starts the HTTP frontend on an ephemeral port and returns its address plus
/// the data directory (kept alive for the test).
fn start_server() -> (std::net::SocketAddr, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let instance = Arc::new(Instance::open(dir.path(), &Config::default()).unwrap());
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

fn request(addr: std::net::SocketAddr, method: &str, path: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn post_query_returns_rows() {
    let (addr, _dir) = start_server();
    let response = request(addr, "POST", "/query", r#"{"sql":"select 1 as one;"}"#);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("\"type\":\"rows\""), "{response}");
    assert!(response.contains("[\"one\"]"), "{response}");
    assert!(response.contains("[[1]]"), "{response}");
}

#[test]
fn post_query_reports_message_and_errors() {
    let (addr, _dir) = start_server();
    let ok = request(addr, "POST", "/query", r#"{"sql":"create table t (id int);"}"#);
    assert!(ok.starts_with("HTTP/1.1 200 OK"), "{ok}");
    assert!(ok.contains("\"type\":\"message\""), "{ok}");

    let bad = request(addr, "POST", "/query", r#"{"sql":"select from;"}"#);
    assert!(bad.starts_with("HTTP/1.1 400 Bad Request"), "{bad}");
    assert!(bad.contains("\"error\""), "{bad}");
}

#[test]
fn health_endpoint_responds() {
    let (addr, _dir) = start_server();
    let response = request(addr, "GET", "/health", "");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("\"status\":\"ok\""), "{response}");
}
