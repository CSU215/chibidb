use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{mpsc, Arc};

use chibidb::config::Config;
use chibidb::instance::Instance;

fn start_server() -> (std::net::SocketAddr, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let instance = Arc::new(Instance::open(dir.path(), &Config::default()).unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let _ = chibidb::mysql::serve(instance, listener).await;
        });
    });
    (rx.recv().unwrap(), dir)
}

fn write_packet(stream: &mut TcpStream, seq: u8, payload: &[u8]) {
    let len = payload.len() as u32;
    stream
        .write_all(&[
            (len & 0xff) as u8,
            ((len >> 8) & 0xff) as u8,
            ((len >> 16) & 0xff) as u8,
            seq,
        ])
        .unwrap();
    stream.write_all(payload).unwrap();
}

fn read_packet(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).unwrap();
    let len = header[0] as usize | (header[1] as usize) << 8 | (header[2] as usize) << 16;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).unwrap();
    payload
}

/// Performs the handshake and returns a connected stream (auth disabled).
fn connect(addr: std::net::SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).unwrap();
    let handshake = read_packet(&mut stream);
    assert_eq!(handshake[0], 10, "expected a protocol v10 handshake");

    let mut response = Vec::new();
    response.extend_from_slice(&0x0000_8201u32.to_le_bytes()); // caps
    response.extend_from_slice(&0u32.to_le_bytes()); // max packet
    response.push(0x21); // charset
    response.extend_from_slice(&[0u8; 23]);
    response.extend_from_slice(b"root\0");
    response.push(0); // empty auth response
    write_packet(&mut stream, 1, &response);

    let ok = read_packet(&mut stream);
    assert_eq!(ok[0], 0x00, "expected OK after handshake, got {ok:?}");
    stream
}

#[test]
fn mysql_query_returns_a_text_result_set() {
    let (addr, _dir) = start_server();
    let mut stream = connect(addr);

    write_packet(&mut stream, 0, b"\x03select 1 as one");
    let column_count = read_packet(&mut stream);
    assert_eq!(column_count, vec![1], "one column expected");
    let _column = read_packet(&mut stream); // column definition
    let eof = read_packet(&mut stream);
    assert_eq!(eof[0], 0xfe, "expected EOF before rows");
    let row = read_packet(&mut stream);
    assert_eq!(row, vec![1, b'1'], "row should be the string \"1\"");
    let end = read_packet(&mut stream);
    assert_eq!(end[0], 0xfe, "expected trailing EOF");
}

#[test]
fn mysql_prepared_statements_bind_parameters() {
    let (addr, _dir) = start_server();
    let mut stream = connect(addr);

    // COM_STMT_PREPARE "select ? + 1 as v"
    let mut prepare = vec![0x16];
    prepare.extend_from_slice(b"select ? + 1 as v");
    write_packet(&mut stream, 0, &prepare);

    let prepare_ok = read_packet(&mut stream);
    assert_eq!(prepare_ok[0], 0x00, "expected prepare OK");
    assert_eq!(u16::from_le_bytes([prepare_ok[5], prepare_ok[6]]), 0, "no columns");
    assert_eq!(u16::from_le_bytes([prepare_ok[7], prepare_ok[8]]), 1, "one param");
    let statement_id = u32::from_le_bytes(prepare_ok[1..5].try_into().unwrap());
    let _param_def = read_packet(&mut stream);
    let _param_eof = read_packet(&mut stream);

    // COM_STMT_EXECUTE with a LONGLONG parameter = 7
    let mut execute = vec![0x17];
    execute.extend_from_slice(&statement_id.to_le_bytes());
    execute.push(0); // flags
    execute.extend_from_slice(&1u32.to_le_bytes()); // iteration
    execute.push(0); // null bitmap (1 byte, none null)
    execute.push(1); // new params bound
    execute.push(8); // MYSQL_TYPE_LONGLONG
    execute.push(0); // unsigned flag
    execute.extend_from_slice(&7i64.to_le_bytes());
    write_packet(&mut stream, 0, &execute);

    let column_count = read_packet(&mut stream);
    assert_eq!(column_count, vec![1]);
    let _column = read_packet(&mut stream);
    let _eof = read_packet(&mut stream);
    let row = read_packet(&mut stream);
    assert_eq!(row, vec![1, b'8'], "expected 7 + 1 = 8");
    let _end = read_packet(&mut stream);

    // COM_STMT_CLOSE is silent
    let mut close = vec![0x19];
    close.extend_from_slice(&statement_id.to_le_bytes());
    write_packet(&mut stream, 0, &close);

    write_packet(&mut stream, 0, &[0x01]); // COM_QUIT
}

#[test]
fn mysql_transaction_control_gets_a_reply() {
    let (addr, _dir) = start_server();
    let mut stream = connect(addr);

    // BEGIN/COMMIT/ROLLBACK produce no result set but still owe an OK packet,
    // otherwise the client blocks waiting for a reply.
    write_packet(&mut stream, 0, b"\x03begin;");
    assert_eq!(read_packet(&mut stream)[0], 0x00, "BEGIN should be acknowledged");
    write_packet(&mut stream, 0, b"\x03commit;");
    assert_eq!(read_packet(&mut stream)[0], 0x00, "COMMIT should be acknowledged");
    write_packet(&mut stream, 0, b"\x03begin;");
    assert_eq!(read_packet(&mut stream)[0], 0x00, "BEGIN should be acknowledged");
    write_packet(&mut stream, 0, b"\x03rollback;");
    assert_eq!(read_packet(&mut stream)[0], 0x00, "ROLLBACK should be acknowledged");

    write_packet(&mut stream, 0, &[0x01]); // COM_QUIT
}

#[test]
fn mysql_reports_errors_and_pings() {
    let (addr, _dir) = start_server();
    let mut stream = connect(addr);

    write_packet(&mut stream, 0, b"\x03select from;");
    let err = read_packet(&mut stream);
    assert_eq!(err[0], 0xff, "expected an error packet");

    write_packet(&mut stream, 0, &[0x0e]); // COM_PING
    let ok = read_packet(&mut stream);
    assert_eq!(ok[0], 0x00);

    write_packet(&mut stream, 0, &[0x01]); // COM_QUIT
}
