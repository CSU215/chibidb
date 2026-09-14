//! `POST /api/plan`: the plan the engine will actually run, not a description
//! of it.
//!
//! `exec/plan.rs` renders EXPLAIN text from the same access-path decision the
//! executor makes, but it is a parallel implementation (`docs/chibidb设计与实现.md`
//! §11.1: "每处改动需要两边同步"). A UI that renders that copy would show a plan
//! nobody runs the moment the two drift. These tests pin the two together.

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

fn admin_config() -> Config {
    Config::from_toml_str("[server]\nadmin_api = true\n").unwrap()
}

fn quote(sql: &str) -> String {
    format!("\"{}\"", sql.replace('"', "\\\""))
}

/// The whole response for one plan request, for the cases that assert on the
/// status code as well as the body.
fn plan_response(addr: std::net::SocketAddr, sql: &str) -> String {
    let payload = "{\"sql\":";
    post(addr, "/api/plan", &format!("{payload}{}}}", quote(sql)))
}

/// The plan report for one statement, as JSON text.
fn plan(addr: std::net::SocketAddr, sql: &str) -> String {
    let response = plan_response(addr, sql);
    assert!(status(&response).contains("200"), "{response}");
    body(&response).to_string()
}

/// Runs setup SQL through the ordinary query endpoint.
fn setup(addr: std::net::SocketAddr, sql: &str) {
    let response = post(addr, "/query", &format!("{{\"sql\":{}}}", quote(sql)));
    assert!(status(&response).contains("200"), "{response}");
}

/// A seeded table with an index on `id`.
fn seeded(addr: std::net::SocketAddr) {
    setup(addr, "create table t (id int, name text);");
    setup(addr, "insert into t values (1,'a'),(2,'b'),(3,'c');");
    setup(addr, "create index idx_id on t (id);");
}

/// The `node` names anywhere in the tree, flattened -- the nesting is not what
/// these tests are checking, only which operators were chosen.
fn node_names(tree: &str) -> Vec<String> {
    const MARKER: &str = "\"node\":\"";
    let mut out = Vec::new();
    let mut rest = tree;
    while let Some(at) = rest.find(MARKER) {
        let after = &rest[at + MARKER.len()..];
        let Some(end) = after.find('"') else { break };
        out.push(after[..end].to_string());
        rest = &after[end..];
    }
    out
}

/// The text between the first `open` and the next `close`.
fn between<'a>(haystack: &'a str, open: &str, close: char) -> &'a str {
    let start = haystack.find(open).expect("open marker") + open.len();
    let rest = &haystack[start..];
    let end = rest.find(close).expect("close marker");
    &rest[..end]
}

#[test]
fn reports_the_operators_that_will_actually_run() {
    let (addr, _dir) = start_server(admin_config());
    seeded(addr);

    // No FROM: a single projected tuple.
    let payload = plan(addr, "select 1 as one;");
    assert!(payload.contains(r#""physical":true"#), "{payload}");
    assert_eq!(node_names(&payload), vec!["Project", "ConstantScan"], "{payload}");

    // No WHERE, no index to ride: a sequential scan.
    let payload = plan(addr, "select name from t;");
    assert_eq!(node_names(&payload), vec!["Project", "TableScan"], "{payload}");

    // Equality on the indexed column: the index, the filter that rechecks what
    // the index could not (visibility), then the projection.
    let payload = plan(addr, "select name from t where id = 5;");
    assert_eq!(node_names(&payload), vec!["Project", "Filter", "IndexScan"], "{payload}");
    assert!(payload.contains(r#""key":"column","value":"id""#), "{payload}");
    // The index name is the planner's record, not the operator's, so it comes
    // from `chosen` rather than from the tree.
    assert!(payload.contains(r#""index":"idx_id""#), "{payload}");

    // ORDER BY the indexed column with no WHERE: the leaf chain already comes
    // out in that order, so this is the ordered variant of the same operator.
    let payload = plan(addr, "select name from t order by id;");
    assert_eq!(node_names(&payload), vec!["Project", "IndexScan"], "{payload}");
    assert!(payload.contains(r#""value":"ordered leaf scan""#), "{payload}");

    // An equi-join hashes rather than nesting loops.
    setup(addr, "create table u (id int, name text);");
    let payload = plan(addr, "select * from t, u where t.id = u.id;");
    assert!(node_names(&payload).contains(&"HashJoin".to_string()), "{payload}");
}

#[test]
fn rejected_candidates_say_why_the_index_won() {
    let (addr, _dir) = start_server(admin_config());
    seeded(addr);

    let payload = plan(addr, "select name from t where id = 5;");
    assert!(payload.contains(r#""chosen":{"path":"IndexScan""#), "{payload}");
    assert!(payload.contains(r#""path":"TableScan""#), "{payload}");
    // The reason has to name the index, or it explains nothing.
    assert!(payload.contains("idx_id"), "{payload}");

    // Without a WHERE clause the index is not a candidate at all, and the
    // rejection says so.
    let payload = plan(addr, "select name from t;");
    assert!(payload.contains(r#""chosen":{"path":"FullScan""#), "{payload}");
    assert!(payload.contains(r#""path":"IndexScan""#), "{payload}");

    // ORDER BY on the indexed column: the ordered scan wins over both.
    let payload = plan(addr, "select name from t order by id;");
    assert!(payload.contains(r#""path":"OrderedIndexScan""#), "{payload}");
}

#[test]
fn the_explain_text_agrees_with_the_operator_tree() {
    let (addr, _dir) = start_server(admin_config());
    seeded(addr);
    setup(addr, "create table u (id int, name text);");
    setup(addr, "create view v as select id, name from t;");

    // The first word of the EXPLAIN text is the access path it claims. Each one
    // maps to the operator that had better be in the real tree.
    let cases: &[(&str, &str)] = &[
        ("select 1 as one;", "ConstantScan"),
        ("select name from t;", "TableScan"),
        ("select name from v;", "ViewScan"),
        ("select name from t where id = 5;", "IndexScan"),
        ("select name from t where id > 1 and id < 3;", "IndexScan"),
        ("select name from t order by id;", "IndexScan"),
        ("select * from t, u where t.id = u.id;", "HashJoin"),
        ("select * from t, u where t.id > u.id;", "NestedLoopJoin"),
    ];
    for (sql, expected) in cases {
        let payload = plan(addr, sql);
        let explain = between(&payload, r#""explain":""#, '"').to_string();
        let claimed = explain.split(['(', ' ']).next().unwrap_or("").to_string();
        let required = match claimed.as_str() {
            "ConstantSelect" => "ConstantScan",
            // A view is scanned by running its own plan, and the tree says so.
            "FullScan" => "TableScan",
            "ViewScan" => "ViewScan",
            "IndexScan" | "OrderedIndexScan" => "IndexScan",
            "HashJoin" => "HashJoin",
            "NestedLoopJoin" => "NestedLoopJoin",
            other => panic!("unknown access path {other:?} in {explain:?}"),
        };
        assert_eq!(required, *expected, "{explain}");
        assert!(
            node_names(&payload).iter().any(|n| n == required),
            "EXPLAIN says {explain:?} but the tree is {:?}",
            node_names(&payload)
        );
        // The text claims the ordered variant only when the operator agrees.
        assert_eq!(
            explain.starts_with("OrderedIndexScan"),
            payload.contains(r#""value":"ordered leaf scan""#),
            "the ordering claim disagrees with the tree: {explain:?}"
        );
    }
}

#[test]
fn bind_info_resolves_the_tables_and_columns() {
    let (addr, _dir) = start_server(admin_config());
    seeded(addr);

    let payload = plan(addr, "select x.name from t x where x.id = 1;");
    // The table reference carries where the rows come from.
    assert!(payload.contains(r#""alias":"x""#), "{payload}");
    assert!(payload.contains(r#""engine":"heap""#), "{payload}");
    assert!(payload.contains(r#""layout":"row""#), "{payload}");
    assert!(payload.contains(r#""file_no":"#), "{payload}");
    // The column reference carries the base column it resolved to.
    assert!(payload.contains(r#""ref":"x.name""#), "{payload}");
    assert!(payload.contains(r#""table":"t","column":"name","type":"text""#), "{payload}");

    // A name that is not a column of any FROM table is reported as unresolved
    // rather than guessed at: the plan itself is still buildable (the failure
    // belongs to execution), but the reader must not be told it resolved.
    let payload = plan(addr, "select nope from t;");
    assert!(payload.contains(r#""ref":"nope""#), "{payload}");
    assert!(payload.contains(r#""source":"unknown""#), "{payload}");
    assert!(payload.contains(r#""table":null,"column":null"#), "{payload}");
}

#[test]
fn a_statement_without_a_plan_is_reported_not_crashed() {
    let (addr, _dir) = start_server(admin_config());

    let response = plan_response(addr, "create table x (y int)");
    assert!(status(&response).contains("200"), "{response}");
    let payload = body(&response).to_string();
    assert!(payload.contains(r#""stage":"plan""#), "{payload}");
    assert!(payload.contains(r#""plan":null"#), "{payload}");

    // A statement that does not parse keeps the same three-way shape as
    // `/api/parse`: a parse failure is a result, not a transport error.
    let response = plan_response(addr, "select (1");
    assert!(status(&response).contains("200"), "{response}");
    assert!(body(&response).contains(r#""stage":"parse""#), "{response}");

    // A malformed request, on the other hand, is a transport error.
    let response = post(addr, "/api/plan", "{}");
    assert!(status(&response).contains("400"), "{response}");
    let response = post(addr, "/api/plan", "not json at all");
    assert!(status(&response).contains("400"), "{response}");
}

#[test]
fn the_endpoint_is_part_of_the_admin_surface() {
    let (addr, _dir) = start_server(Config::default());
    let response = plan_response(addr, "select 1;");
    assert!(status(&response).contains("404"), "{response}");
}
