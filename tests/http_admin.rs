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

/// Posts a raw body to a path, over its own connection.
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

fn admin_config() -> Config {
    Config::from_toml_str("[server]\nadmin_api = true\n").unwrap()
}

/// The byte offset the trace pointed at, or `None` if it said `null`.
///
/// Starts from the `error` object on purpose: every token also carries a `pos`,
/// so searching the whole payload finds a token's offset and quietly reports it
/// as the error's.
fn reported_pos(payload: &str) -> Option<usize> {
    let rest = &payload[payload.find(r#""error":"#)?..];
    let needle = r#""pos":"#;
    let rest = &rest[rest.find(needle)? + needle.len()..];
    if rest.starts_with("null") {
        return None;
    }
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Parses `sql` and returns where the trace pointed.
fn parse_pos(addr: std::net::SocketAddr, sql: &str) -> Option<usize> {
    let response = post(addr, "/api/parse", &format!("{{\"sql\":\"{sql}\"}}"));
    reported_pos(body(&response))
}

#[test]
fn a_syntax_error_points_at_the_offending_token() {
    let (addr, _dir) = start_server(admin_config());

    // Each case is asserted by the character found at the reported offset, so
    // the expectation states what the position *means* rather than restating an
    // index that was read off the implementation.
    for (sql, expected) in [
        ("select 99999999999999999999;", '9'),
        ("create table t (c char(0));", '0'),
        ("select 1 limit -1;", '-'),
        ("select 1 like 2 escape 3;", '3'),
        ("select 1 +;", ' '),
    ] {
        let pos = parse_pos(addr, sql).unwrap_or_else(|| panic!("no position for {sql:?}"));
        let at = sql[pos..].chars().next();
        if expected == ' ' {
            // Nothing to point at: the terminator was consumed, so the cursor is
            // the honest answer, and that is the end of the statement.
            assert_eq!(pos, sql.len(), "{sql:?} -> {pos}");
        } else {
            assert_eq!(at, Some(expected), "{sql:?} -> pos {pos}, char {at:?}");
        }
    }
}

#[test]
fn an_unfinished_statement_points_at_its_end() {
    let (addr, _dir) = start_server(admin_config());

    // The complaint ("expected punctuation") is about a `)` that was never
    // written, so there is no token to blame; the end of the input is where the
    // user's cursor is.
    let sql = "create table t (id int";
    assert_eq!(parse_pos(addr, sql), Some(sql.len()));
}

#[test]
fn a_runtime_only_problem_reports_no_position() {
    let (addr, _dir) = start_server(admin_config());

    // `select from;` compiles, so /api/parse has nothing to point at. The
    // distinction matters: the console falls back to a banner for these, and a
    // position here would make it draw a marker somewhere arbitrary.
    assert_eq!(parse_pos(addr, "select from;"), None);
}

#[test]
fn a_clean_statement_reports_no_error() {
    let (addr, _dir) = start_server(admin_config());

    let response = post(addr, "/api/parse", r#"{"sql":"select 1 as one;"}"#);
    assert!(body(&response).contains(r#""error":null"#), "{response}");
    assert_eq!(reported_pos(body(&response)), None);
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
fn the_parse_endpoint_is_off_unless_switched_on() {
    let (addr, _dir) = start_server(Config::default());

    let response = post(addr, "/api/parse", r#"{"sql":"select 1;"}"#);
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
}

#[test]
fn the_parse_endpoint_returns_tokens_and_the_ast() {
    let (addr, _dir) = start_server(admin_config());

    let response = post(addr, "/api/parse", r#"{"sql":"select 1;"}"#);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""kind":"Ident","text":"select","pos":0"#), "{payload}");
    assert!(payload.contains(r#""kind":"Punct","text":";""#), "{payload}");
    assert!(payload.contains(r#""statements":[{"debug":"#), "{payload}");
    assert!(payload.contains(r#""error":null"#), "{payload}");
    // Running the statement is explicitly not part of this.
    assert!(!payload.contains("SUCCESS"), "{payload}");
}

#[test]
fn a_parse_error_is_a_result_not_a_transport_error() {
    let (addr, _dir) = start_server(admin_config());

    // 200, not 400: this endpoint diagnoses text the user is still typing, so
    // failing to parse is an answer. The caller reads `error`, not the status.
    let response = post(addr, "/api/parse", r#"{"sql":"select 1 +;"}"#);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""stage":"parse""#), "{payload}");
    assert!(payload.contains("expected expression"), "{payload}");
    // The token stream is still useful when parsing fails, which is the point
    // of reporting the two stages separately.
    assert!(payload.contains(r#""text":"select""#), "{payload}");
    assert!(payload.contains(r#""statements":[]"#), "{payload}");
}

#[test]
fn the_parse_endpoint_reports_syntax_only_not_semantics() {
    let (addr, _dir) = start_server(admin_config());

    // `select from;` is not a syntax error here: keywords are identifiers, so
    // this parses as a select of a column named `from`, and only fails when it
    // is *run* (`no such column`). Pinning that makes the division of labour
    // explicit -- /api/parse answers "does this compile", and running the
    // statement is still the only way to learn whether it means anything.
    let response = post(addr, "/api/parse", r#"{"sql":"select from;"}"#);
    let payload = body(&response);
    assert!(payload.contains(r#""error":null"#), "{payload}");

    let run = query(addr, None, "select from;");
    assert!(run.starts_with("HTTP/1.1 400"), "{run}");
    assert!(body(&run).contains("no such column"), "{run}");
}

#[test]
fn a_lex_error_reports_its_own_stage() {
    let (addr, _dir) = start_server(admin_config());

    // An integer literal too large for i64 fails in the lexer, before parsing,
    // so there is no token stream to report.
    let response = post(addr, "/api/parse", r#"{"sql":"select 99999999999999999999;"}"#);
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let payload = body(&response);
    assert!(payload.contains(r#""stage":"lex""#), "{payload}");
    assert!(payload.contains(r#""tokens":[]"#), "{payload}");
}

#[test]
fn a_malformed_request_body_is_a_400() {
    let (addr, _dir) = start_server(admin_config());

    // Not JSON at all.
    let response = post(addr, "/api/parse", "select 1;");
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(body(&response).contains("error"), "{response}");

    // JSON, but without a usable `sql`.
    let response = post(addr, "/api/parse", r#"{"query":"select 1;"}"#);
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");

    let response = post(addr, "/api/parse", r#"{"sql":42}"#);
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
}

#[test]
fn unknown_api_paths_are_404() {
    let (addr, _dir) = start_server(admin_config());

    let response = post(addr, "/api/nope", "{}");
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
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
