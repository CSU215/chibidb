//! Cross-request sessions for the HTTP frontend.
//!
//! The frontend keeps one `Session` per TCP connection, which a browser cannot
//! rely on: it pools and reuses sockets freely, so `BEGIN` on one request and
//! `INSERT` on the next may land on different connections. A client that sends
//! `X-Chibi-Session` gets a session that outlives the connection instead.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{mpsc, Arc};

use chibidb::config::Config;
use chibidb::instance::Instance;

const SESSION_HEADER: &str = "x-chibi-session";

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

/// Runs one statement over its own connection, optionally claiming a session.
/// Every request closes its connection, which is what makes the test meaningful:
/// the session has to survive the socket going away.
fn query(addr: std::net::SocketAddr, session: Option<&str>, sql: &str) -> String {
    let body = format!("{{\"sql\":\"{sql}\"}}");
    let mut request = format!(
        "POST /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(id) = session {
        request.push_str(&format!("X-Chibi-Session: {id}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&body);

    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

/// Fetches a path over its own connection.
fn get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

/// The session id the server handed back, if any.
fn session_id(response: &str) -> Option<String> {
    response.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim().eq_ignore_ascii_case(SESSION_HEADER).then(|| value.trim().to_string())
    })
}

fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}

#[test]
fn a_session_outlives_the_connection_that_started_it() {
    let (addr, _dir) = start_server(Config::default());

    let setup = query(addr, None, "create table t (id int);");
    assert!(setup.starts_with("HTTP/1.1 200 OK"), "{setup}");

    // A fresh page picks up an id without running a statement.
    let id = session_id(&get(addr, "/session")).expect("the server must mint a session id");

    // An uncommitted insert, on a connection that then closes.
    let open = query(addr, Some(&id), "begin; insert into t values (1);");
    assert!(open.starts_with("HTTP/1.1 200 OK"), "{open}");

    // Same session, brand new connection: the open transaction is still there,
    // so its own uncommitted row is visible to it.
    let same = query(addr, Some(&id), "select * from t;");
    assert!(body(&same).contains("[[1]]"), "{same}");

    // A different session sees nothing: the row is not committed. This is the
    // control that proves the previous assertion came from session continuity
    // and not from the row simply being committed.
    let other = query(addr, None, "select * from t;");
    assert!(!body(&other).contains("[[1]]"), "{other}");
}

#[test]
fn closing_a_connection_still_rolls_back_its_own_session() {
    let (addr, _dir) = start_server(Config::default());

    let setup = query(addr, None, "create table t (id int);");
    assert!(setup.starts_with("HTTP/1.1 200 OK"), "{setup}");

    // No session header: this runs on the per-connection session, which is
    // rolled back when the socket closes. Adding the registry must not disturb
    // that, or a client that never sends the header would leak transactions.
    query(addr, None, "begin; insert into t values (7);");

    let after = query(addr, None, "select * from t;");
    assert!(!body(&after).contains("[[7]]"), "{after}");
}

#[test]
fn an_unknown_session_id_gets_a_fresh_session() {
    let (addr, _dir) = start_server(Config::default());

    // A stale id from a previous server run must not be trusted, and must not
    // be an error either: hand back a working session with a new id.
    let response = query(addr, Some("not-a-real-session"), "select 1 as one;");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let issued = session_id(&response).expect("a replacement id must be issued");
    assert_ne!(issued, "not-a-real-session", "{response}");

    // And the id the client asked for is not usable afterwards, so a client
    // cannot name its own session.
    let again = query(addr, Some("not-a-real-session"), "select 1 as one;");
    assert_ne!(session_id(&again).as_deref(), Some("not-a-real-session"), "{again}");
}

#[test]
fn a_request_without_the_header_gets_no_session_id() {
    let (addr, _dir) = start_server(Config::default());

    // Handing out an id to every stateless request would make the registry
    // churn -- one entry per curl or health probe -- and every entry costs a
    // sweeper slot. Only clients that opt in should get one.
    let response = query(addr, None, "select 1 as one;");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(session_id(&response).is_none(), "{response}");
}
