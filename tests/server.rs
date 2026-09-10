use std::sync::Arc;

use chibidb::result::ResultSet;
use chibidb::server::{serve, SharedDb};
use chibidb::value::Value;
use chibidb::{Database, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

async fn start_server() -> (SharedDb, std::net::SocketAddr, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let shared: SharedDb = Arc::new(Mutex::new(db));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(shared.clone(), listener));
    (shared, addr, dir)
}

/// Sends one statement and collects frames until Done.
async fn exec_remote(stream: &mut TcpStream, sql: &str) -> Result<Vec<ResultSet>> {
    let bytes = sql.as_bytes();
    stream.write_all(&(bytes.len() as u32).to_le_bytes()).await.unwrap();
    stream.write_all(bytes).await.unwrap();

    let mut out = Vec::new();
    let mut failure = None;
    loop {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await.unwrap();
        match chibidb::wire::decode_frame(&body).unwrap() {
            chibidb::wire::Frame::Message(m) => out.push(ResultSet::Message(m)),
            chibidb::wire::Frame::Error(e) => failure = Some(chibidb::Error::Runtime(e)),
            chibidb::wire::Frame::Rows(rs) => out.push(rs),
            chibidb::wire::Frame::Done => {
                match failure {
                    Some(e) => return Err(e),
                    None => return Ok(out),
                }
            }
        }
    }
}

#[tokio::test]
async fn sessions_share_the_database() {
    let (_shared, addr, _dir) = start_server().await;

    let mut client1 = TcpStream::connect(addr).await.unwrap();
    let rs = exec_remote(&mut client1, "create table t (id int, name char(8));")
        .await
        .unwrap();
    assert_eq!(rs, [ResultSet::Message("SUCCESS".into())]);

    let rs = exec_remote(
        &mut client1,
        "insert into t values (1, 'alice'), (2, 'bob');",
    )
    .await
    .unwrap();
    assert_eq!(rs, [ResultSet::Message("SUCCESS".into())]);

    // a second connection observes the same database
    let mut client2 = TcpStream::connect(addr).await.unwrap();
    let rs = exec_remote(&mut client2, "select * from t;").await.unwrap();
    match &rs[0] {
        ResultSet::Rows { columns, rows } => {
            assert_eq!(columns.as_slice(), ["id", "name"]);
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0], [Value::Int(1), Value::Str("alice".into())]);
        }
        other => panic!("expected rows, got {other:?}"),
    }

    // errors travel back without killing the session
    let err = exec_remote(&mut client1, "select * from missing;").await.unwrap_err();
    assert!(err.to_string().contains("no such table"), "{err}");
    let rs = exec_remote(&mut client1, "select count(*) from t;").await.unwrap();
    match &rs[0] {
        ResultSet::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(2)),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[tokio::test]
async fn transactions_span_statements_on_one_connection() {
    let (_shared, addr, _dir) = start_server().await;

    let mut a = TcpStream::connect(addr).await.unwrap();
    let mut b = TcpStream::connect(addr).await.unwrap();

    exec_remote(&mut a, "create table t (id int);").await.unwrap();

    exec_remote(&mut a, "begin;").await.unwrap();
    exec_remote(&mut a, "insert into t values (1);").await.unwrap();

    // the writer sees its own uncommitted row...
    let rs = exec_remote(&mut a, "select count(*) from t;").await.unwrap();
    match &rs[0] {
        ResultSet::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(1)),
        other => panic!("expected rows, got {other:?}"),
    }
    // ...but the insert must not have autocommitted: b sees nothing yet
    let rs = exec_remote(&mut b, "select count(*) from t;").await.unwrap();
    match &rs[0] {
        ResultSet::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(0)),
        other => panic!("expected rows, got {other:?}"),
    }

    exec_remote(&mut a, "commit;").await.unwrap();
    let rs = exec_remote(&mut b, "select count(*) from t;").await.unwrap();
    match &rs[0] {
        ResultSet::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(1)),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[tokio::test]
async fn rolled_back_transaction_leaves_no_rows() {
    let (_shared, addr, _dir) = start_server().await;

    let mut a = TcpStream::connect(addr).await.unwrap();
    exec_remote(&mut a, "create table t (id int);").await.unwrap();
    exec_remote(&mut a, "begin;").await.unwrap();
    exec_remote(&mut a, "insert into t values (1);").await.unwrap();
    exec_remote(&mut a, "rollback;").await.unwrap();
    let rs = exec_remote(&mut a, "select count(*) from t;").await.unwrap();
    match &rs[0] {
        ResultSet::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(0)),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[tokio::test]
async fn concurrent_statements_serialize() {
    let (_shared, addr, _dir) = start_server().await;

    let mut a = TcpStream::connect(addr).await.unwrap();
    let mut b = TcpStream::connect(addr).await.unwrap();

    exec_remote(&mut a, "create table t (id int);").await.unwrap();
    for i in 0..20 {
        exec_remote(&mut a, &format!("insert into t values ({i});")).await.unwrap();
    }
    for i in 100..120 {
        exec_remote(&mut b, &format!("insert into t values ({i});")).await.unwrap();
    }
    let rs = exec_remote(&mut a, "select count(*) from t;").await.unwrap();
    match &rs[0] {
        ResultSet::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(40)),
        other => panic!("expected rows, got {other:?}"),
    }
}
