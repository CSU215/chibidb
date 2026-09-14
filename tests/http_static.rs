//! Static hosting for the HTTP frontend: serves the built Vue SPA (`web/dist`)
//! from the same origin as `/query` and `/health`.
//!
//! Shares the connection helpers with `http_frontend.rs` (separate test crates
//! cannot share code, so each keeps its own copy).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::{mpsc, Arc};

use chibidb::config::Config;
use chibidb::instance::Instance;

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
            let _ = chibidb::http::serve(instance, listener).await;
        });
    });
    (rx.recv().unwrap(), dir)
}

/// Builds a fake site: `<dir>/web/` is the web root, and `<dir>/secret.txt` sits
/// *outside* it as a sentinel proving that traversal is blocked.
fn web_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("secret.txt"), "TOP-SECRET-SENTINEL").unwrap();
    let web = dir.path().join("web");
    std::fs::create_dir_all(web.join("assets")).unwrap();
    std::fs::write(web.join("index.html"), "<!doctype html><title>chibidb</title>").unwrap();
    std::fs::write(web.join("assets/app.js"), "console.log(1)").unwrap();
    dir
}

/// The web root itself (`web_fixture`'s `web/` subdirectory).
fn web_root(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("web")
}

fn config_with_web_root(root: &Path) -> Config {
    Config::from_toml_str(&format!("[server]\nweb_root = \"{}\"\n", root.display())).unwrap()
}

fn request(addr: std::net::SocketAddr, method: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    let req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn serves_the_spa_index_and_assets() {
    let fixture = web_fixture();
    let (addr, _data) = start_server(config_with_web_root(&web_root(&fixture)));

    let index = request(addr, "GET", "/");
    assert!(index.starts_with("HTTP/1.1 200 OK"), "{index}");
    assert!(index.contains("Content-Type: text/html"), "{index}");
    assert!(index.contains("<!doctype html>"), "{index}");
    // index.html must not be cached, or a fresh build would keep serving the old
    // asset hashes.
    assert!(index.contains("Cache-Control: no-cache"), "{index}");

    let asset = request(addr, "GET", "/assets/app.js");
    assert!(asset.starts_with("HTTP/1.1 200 OK"), "{asset}");
    assert!(asset.contains("Content-Type: application/javascript"), "{asset}");
    // Hashed assets can be cached forever.
    assert!(asset.contains("Cache-Control: max-age=31536000, immutable"), "{asset}");
    assert!(asset.contains("console.log(1)"), "{asset}");
}

#[test]
fn missing_assets_are_404() {
    let fixture = web_fixture();
    let (addr, _data) = start_server(config_with_web_root(&web_root(&fixture)));

    let response = request(addr, "GET", "/assets/missing.js");
    assert!(response.starts_with("HTTP/1.1 404 Not Found"), "{response}");
}

#[test]
fn path_traversal_is_rejected() {
    let fixture = web_fixture();
    let (addr, _data) = start_server(config_with_web_root(&web_root(&fixture)));

    // A bare `..`: a well-behaved client would not send this, a hostile one would.
    let dotted = request(addr, "GET", "/../secret.txt");
    assert!(dotted.starts_with("HTTP/1.1 404"), "{dotted}");
    assert!(!dotted.contains("TOP-SECRET-SENTINEL"), "{dotted}");

    // Percent-encoding: rejected outright rather than decoded, which is simpler
    // and immune to `%2e%2e` by construction.
    let encoded = request(addr, "GET", "/..%2fsecret.txt");
    assert!(encoded.starts_with("HTTP/1.1 404"), "{encoded}");
    assert!(!encoded.contains("TOP-SECRET-SENTINEL"), "{encoded}");

    // Asking for the file by its real name, outside the root.
    let absolute = request(addr, "GET", "/secret.txt");
    assert!(absolute.starts_with("HTTP/1.1 404"), "{absolute}");
    assert!(!absolute.contains("TOP-SECRET-SENTINEL"), "{absolute}");
}

#[test]
fn the_query_string_is_not_part_of_the_file_name() {
    let fixture = web_fixture();
    let (addr, _data) = start_server(config_with_web_root(&web_root(&fixture)));

    // Without stripping at `?` this would look for a file literally named "?v=1".
    let response = request(addr, "GET", "/assets/app.js?v=1");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("console.log(1)"), "{response}");
}

#[test]
fn the_existing_endpoints_still_work() {
    let fixture = web_fixture();
    let (addr, _data) = start_server(config_with_web_root(&web_root(&fixture)));

    let health = request(addr, "GET", "/health");
    assert!(health.starts_with("HTTP/1.1 200 OK"), "{health}");
    assert!(health.contains("Content-Type: application/json"), "{health}");
    assert!(health.contains("\"status\":\"ok\""), "{health}");
}

#[test]
fn an_empty_web_root_serves_nothing() {
    let (addr, _data) = start_server(Config::from_toml_str("[server]\nweb_root = \"\"\n").unwrap());

    // Hosting is off, so `/` is not a site index; it must not answer 200 either.
    let response = request(addr, "GET", "/");
    assert!(!response.starts_with("HTTP/1.1 200 OK"), "{response}");
}

#[test]
fn a_missing_web_root_explains_how_to_build_the_spa() {
    // A fresh clone has no web/dist, so this is the common case, not an edge one.
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no-such-web-root");
    let (addr, _data) = start_server(config_with_web_root(&missing));

    let response = request(addr, "GET", "/");
    // 200, not 404: browsers hide the body of a 404, and this body is the point.
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("Content-Type: text/html"), "{response}");
    // It must say what to do, and where it looked.
    assert!(response.contains("build_web.sh"), "{response}");
    assert!(response.contains("no-such-web-root"), "{response}");

    // There is no site, so assets are still missing rather than a hint page.
    let asset = request(addr, "GET", "/assets/app.js");
    assert!(asset.starts_with("HTTP/1.1 404"), "{asset}");
}
