//! Minimal MySQL wire-protocol frontend.
//!
//! Enough of protocol 4.1 for a `mysql` client to connect: the handshake with
//! `mysql_native_password`, `COM_QUERY` in the text protocol, and
//! `COM_PING`/`COM_INIT_DB`/`COM_QUIT`. Hand-rolled (including a small SHA-1
//! for the native challenge) so no dependency is added.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::net::TcpListener;

use crate::config::Isolation;
use crate::result::ResultSet;
use crate::server::SharedInstance;
use crate::trx::Session;
use crate::value::Value;

// Capability flags we advertise.
const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_TRANSACTIONS: u32 = 0x0000_2000;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
const CAPABILITIES: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_CONNECT_WITH_DB
    | CLIENT_PROTOCOL_41
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_PLUGIN_AUTH;

/// Status flag: autocommit.
const STATUS_AUTOCOMMIT: u16 = 0x0002;
/// Status flag: another result set follows.
const STATUS_MORE_RESULTS: u16 = 0x0008;

const COM_QUIT: u8 = 0x01;
const COM_INIT_DB: u8 = 0x02;
const COM_QUERY: u8 = 0x03;
const COM_PING: u8 = 0x0e;
const COM_STMT_PREPARE: u8 = 0x16;
const COM_STMT_EXECUTE: u8 = 0x17;
const COM_STMT_CLOSE: u8 = 0x19;
const COM_STMT_RESET: u8 = 0x1a;

// Parameter type codes we understand.
const MYSQL_TYPE_TINY: u8 = 1;
const MYSQL_TYPE_SHORT: u8 = 2;
const MYSQL_TYPE_LONG: u8 = 3;
const MYSQL_TYPE_FLOAT: u8 = 4;
const MYSQL_TYPE_DOUBLE: u8 = 5;
const MYSQL_TYPE_NULL: u8 = 6;
const MYSQL_TYPE_LONGLONG: u8 = 8;
const MYSQL_TYPE_DATE: u8 = 10;
const MYSQL_TYPE_STRING: u8 = 0xfe;
const MYSQL_TYPE_VAR_STRING: u8 = 0xfd;

/// Accepts MySQL connections until the listener closes.
pub async fn serve(instance: SharedInstance, listener: TcpListener) -> io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let stream = stream.into_std()?;
        stream.set_nonblocking(false)?;
        let instance = instance.clone();
        std::thread::spawn(move || {
            if let Err(e) = serve_connection(&instance, stream) {
                eprintln!("mysql connection {peer} error: {e}");
            }
        });
    }
}

fn serve_connection(instance: &crate::instance::Instance, mut stream: TcpStream) -> io::Result<()> {
    static NEXT_ID: AtomicU32 = AtomicU32::new(1);
    let conn_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let scramble = random_scramble();

    write_packet(&mut stream, 0, &handshake_packet(conn_id, &scramble))?;
    let (_, response) = read_packet(&mut stream)?;
    let Some(hs) = parse_handshake_response(&response) else {
        return Ok(());
    };

    let authenticated = if instance.config().auth.enabled {
        let verifier = instance
            .native_verifier(&hs.user)
            .map_err(|e| io::Error::other(e.to_string()))?;
        verifier
            .map(|v| native_password_ok(&v, &scramble, &hs.auth))
            .unwrap_or(false)
    } else {
        true
    };
    if !authenticated {
        write_packet(&mut stream, 2, &err_packet(1045, "Access denied"))?;
        return Ok(());
    }

    let mut session = Session::new();
    if !hs.user.is_empty() {
        session.set_user(Some(hs.user.clone()));
    }
    if let Some(db) = &hs.database
        && let Err(e) = instance.execute_with(&mut session, &format!("use {db};"))
    {
        write_packet(&mut stream, 2, &error_packet(&e.to_string()))?;
        return Ok(());
    }
    write_packet(&mut stream, 2, &ok_packet(session_status(&session)))?;

    let mut prepared: HashMap<u32, String> = HashMap::new();
    let mut next_statement_id: u32 = 1;

    while let Ok((_, packet)) = read_packet(&mut stream) {
        let Some((&command, payload)) = packet.split_first() else {
            continue;
        };
        match command {
            COM_QUIT => break,
            COM_PING => write_packet(&mut stream, 1, &ok_packet(session_status(&session)))?,
            COM_INIT_DB => {
                let db = String::from_utf8_lossy(payload);
                // USE is a database statement: MySQL implicitly commits first.
                if let Err(e) = commit_if_open(instance, &mut session) {
                    write_packet(&mut stream, 1, &error_packet(&e.to_string()))?;
                    continue;
                }
                match instance.execute_with(&mut session, &format!("use {db};")) {
                    Ok(_) => write_packet(&mut stream, 1, &ok_packet(session_status(&session)))?,
                    Err(e) => write_packet(&mut stream, 1, &error_packet(&e.to_string()))?,
                }
            }
            COM_QUERY => handle_query(instance, &mut stream, &mut session, payload, conn_id)?,
            COM_STMT_PREPARE => {
                let sql = String::from_utf8_lossy(payload).into_owned();
                let id = next_statement_id;
                next_statement_id += 1;
                let params = placeholder_count(&sql);
                prepared.insert(id, sql);
                let mut seq = 1u8;
                write_packet(&mut stream, seq, &prepare_ok_packet(id, params))?;
                seq = seq.wrapping_add(1);
                for _ in 0..params {
                    write_packet(&mut stream, seq, &column_definition("?", MYSQL_TYPE_VAR_STRING))?;
                    seq = seq.wrapping_add(1);
                }
                if params > 0 {
                    write_packet(&mut stream, seq, &eof_packet(session_status(&session)))?;
                }
            }
            COM_STMT_EXECUTE => {
                let mut pos = 0;
                let Some(id) = read_u32(payload, &mut pos) else {
                    write_packet(&mut stream, 1, &err_packet(1064, "bad execute packet"))?;
                    continue;
                };
                let Some(sql) = prepared.get(&id).cloned() else {
                    write_packet(&mut stream, 1, &err_packet(1243, "unknown prepared statement"))?;
                    continue;
                };
                let params = placeholder_count(&sql);
                let Some(values) = parse_execute(payload, params) else {
                    write_packet(&mut stream, 1, &err_packet(1064, "bad parameters"))?;
                    continue;
                };
                let bound = bind_params(&sql, &values);
                match instance.execute_with(&mut session, &bound) {
                    Ok(results) => {
                        send_results(&mut stream, &results, session_status(&session))?
                    }
                    Err(e) => write_packet(&mut stream, 1, &error_packet(&e.to_string()))?,
                }
            }
            COM_STMT_CLOSE => {
                let mut pos = 0;
                if let Some(id) = read_u32(payload, &mut pos) {
                    prepared.remove(&id);
                }
            }
            COM_STMT_RESET => write_packet(&mut stream, 1, &ok_packet(session_status(&session)))?,
            _ => write_packet(&mut stream, 1, &err_packet(1047, "unsupported command"))?,
        }
    }

    let _ = instance.rollback_session(&mut session);
    Ok(())
}

struct HandshakeResponse {
    user: String,
    auth: Vec<u8>,
    database: Option<String>,
}

fn parse_handshake_response(payload: &[u8]) -> Option<HandshakeResponse> {
    let mut pos = 0;
    let capabilities = read_u32(payload, &mut pos)?;
    pos += 4; // max packet size
    pos += 1; // charset
    pos += 23; // reserved
    let user = read_nul_string(payload, &mut pos)?;
    let auth = if capabilities & CLIENT_SECURE_CONNECTION != 0 {
        let len = *payload.get(pos)? as usize;
        pos += 1;
        let bytes = payload.get(pos..pos + len)?.to_vec();
        pos += len;
        bytes
    } else {
        read_nul_bytes(payload, &mut pos)?
    };
    let database = if capabilities & CLIENT_CONNECT_WITH_DB != 0 {
        read_nul_string(payload, &mut pos)
    } else {
        None
    };
    Some(HandshakeResponse { user, auth, database })
}

fn send_results(stream: &mut TcpStream, results: &[ResultSet], base: u16) -> io::Result<()> {
    let mut seq = 1u8;
    for (i, result) in results.iter().enumerate() {
        let more = i + 1 < results.len();
        let status = base | if more { STATUS_MORE_RESULTS } else { 0 };
        match result {
            ResultSet::Message(_) => {
                write_packet(stream, seq, &ok_packet(status))?;
                seq = seq.wrapping_add(1);
            }
            ResultSet::Affected(n) => {
                write_packet(stream, seq, &ok_packet_affected(status, *n))?;
                seq = seq.wrapping_add(1);
            }
            ResultSet::Rows { columns, rows } => {
                let mut count = Vec::new();
                lenenc_int(&mut count, columns.len() as u64);
                write_packet(stream, seq, &count)?;
                seq = seq.wrapping_add(1);
                let types = column_types(rows, columns.len());
                for (column, &ty) in columns.iter().zip(&types) {
                    write_packet(stream, seq, &column_definition(column, ty))?;
                    seq = seq.wrapping_add(1);
                }
                write_packet(stream, seq, &eof_packet(status))?;
                seq = seq.wrapping_add(1);
                for row in rows {
                    write_packet(stream, seq, &row_packet(row))?;
                    seq = seq.wrapping_add(1);
                }
                write_packet(stream, seq, &eof_packet(status))?;
                seq = seq.wrapping_add(1);
            }
        }
    }
    // A statement with no result set (BEGIN/COMMIT/ROLLBACK...) still owes the
    // client one packet, or it waits forever for a reply.
    if results.is_empty() {
        write_packet(stream, seq, &ok_packet(base))?;
    }
    Ok(())
}

/// A bound parameter value from `COM_STMT_EXECUTE`.
enum ParamValue {
    Int(i64),
    Float(f64),
    Str(String),
}

/// Counts `?` placeholders outside single-quoted strings.
fn placeholder_count(sql: &str) -> usize {
    let mut count = 0;
    let mut in_string = false;
    for c in sql.chars() {
        match c {
            '\'' => in_string = !in_string,
            '?' if !in_string => count += 1,
            _ => {}
        }
    }
    count
}

/// Substitutes parameter literals for the `?` placeholders.
fn bind_params(sql: &str, values: &[Option<ParamValue>]) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut index = 0;
    let mut in_string = false;
    for c in sql.chars() {
        if c == '\'' {
            in_string = !in_string;
            out.push(c);
            continue;
        }
        if c == '?' && !in_string {
            match values.get(index) {
                Some(Some(ParamValue::Int(n))) => out.push_str(&n.to_string()),
                Some(Some(ParamValue::Float(x))) => out.push_str(&x.to_string()),
                Some(Some(ParamValue::Str(s))) => {
                    out.push('\'');
                    out.push_str(s);
                    out.push('\'');
                }
                _ => out.push_str("NULL"),
            }
            index += 1;
            continue;
        }
        out.push(c);
    }
    out
}

fn prepare_ok_packet(statement_id: u32, params: usize) -> Vec<u8> {
    let mut p = vec![0x00];
    p.extend_from_slice(&statement_id.to_le_bytes());
    p.extend_from_slice(&0u16.to_le_bytes()); // column count
    p.extend_from_slice(&(params as u16).to_le_bytes());
    p.push(0); // reserved filler
    p.extend_from_slice(&0u16.to_le_bytes()); // warning count
    p
}

fn parse_execute(payload: &[u8], num_params: usize) -> Option<Vec<Option<ParamValue>>> {
    let mut pos = 0;
    read_u32(payload, &mut pos)?; // statement id
    pos += 1; // flags
    pos += 4; // iteration count
    let null_len = num_params.div_ceil(8);
    let nulls = payload.get(pos..pos + null_len)?.to_vec();
    pos += null_len;
    let new_bound = *payload.get(pos)?;
    pos += 1;

    let mut types = vec![0u8; num_params];
    if new_bound == 1 {
        for slot in types.iter_mut() {
            *slot = *payload.get(pos)?;
            pos += 2; // type + unsigned flag
        }
    }

    let mut values = Vec::with_capacity(num_params);
    for (i, &ty) in types.iter().enumerate() {
        let is_null = nulls[i / 8] & (1 << (i % 8)) != 0 || ty == MYSQL_TYPE_NULL;
        if is_null {
            values.push(None);
            continue;
        }
        let value = match ty {
            MYSQL_TYPE_TINY => {
                let byte = *payload.get(pos)?;
                pos += 1;
                ParamValue::Int(byte as i8 as i64)
            }
            MYSQL_TYPE_SHORT => {
                let b = payload.get(pos..pos + 2)?;
                pos += 2;
                ParamValue::Int(i16::from_le_bytes(b.try_into().unwrap()) as i64)
            }
            MYSQL_TYPE_LONG => {
                let b = payload.get(pos..pos + 4)?;
                pos += 4;
                ParamValue::Int(i32::from_le_bytes(b.try_into().unwrap()) as i64)
            }
            MYSQL_TYPE_LONGLONG => {
                let b = payload.get(pos..pos + 8)?;
                pos += 8;
                ParamValue::Int(i64::from_le_bytes(b.try_into().unwrap()))
            }
            MYSQL_TYPE_FLOAT => {
                let b = payload.get(pos..pos + 4)?;
                pos += 4;
                ParamValue::Float(f32::from_le_bytes(b.try_into().unwrap()) as f64)
            }
            MYSQL_TYPE_DOUBLE => {
                let b = payload.get(pos..pos + 8)?;
                pos += 8;
                ParamValue::Float(f64::from_le_bytes(b.try_into().unwrap()))
            }
            MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING => {
                let (bytes, next) = read_lenenc_bytes(payload, pos)?;
                pos = next;
                ParamValue::Str(String::from_utf8_lossy(&bytes).into_owned())
            }
            _ => return None,
        };
        values.push(Some(value));
    }
    Some(values)
}

fn read_lenenc_bytes(data: &[u8], mut pos: usize) -> Option<(Vec<u8>, usize)> {
    let first = *data.get(pos)?;
    pos += 1;
    let len = match first {
        0..=250 => first as usize,
        0xfc => {
            let b = data.get(pos..pos + 2)?;
            pos += 2;
            u16::from_le_bytes(b.try_into().unwrap()) as usize
        }
        0xfd => {
            let b = data.get(pos..pos + 3)?;
            pos += 3;
            b[0] as usize | (b[1] as usize) << 8 | (b[2] as usize) << 16
        }
        0xfe => {
            let b = data.get(pos..pos + 8)?;
            pos += 8;
            u64::from_le_bytes(b.try_into().unwrap()) as usize
        }
        _ => return None,
    };
    let end = pos.checked_add(len)?;
    let bytes = data.get(pos..end)?.to_vec();
    Some((bytes, end))
}

fn handshake_packet(connection_id: u32, scramble: &[u8; 20]) -> Vec<u8> {
    let mut p = Vec::new();
    p.push(10); // protocol version
    p.extend_from_slice(b"8.0.0-chibidb\0");
    p.extend_from_slice(&connection_id.to_le_bytes());
    p.extend_from_slice(&scramble[0..8]);
    p.push(0);
    p.extend_from_slice(&(CAPABILITIES as u16).to_le_bytes());
    p.push(0x21); // utf8_general_ci
    p.extend_from_slice(&STATUS_AUTOCOMMIT.to_le_bytes());
    p.extend_from_slice(&((CAPABILITIES >> 16) as u16).to_le_bytes());
    p.push(21); // auth plugin data length
    p.extend_from_slice(&[0u8; 10]);
    p.extend_from_slice(&scramble[8..20]);
    p.push(0);
    p.extend_from_slice(b"mysql_native_password\0");
    p
}

fn ok_packet(status: u16) -> Vec<u8> {
    let mut p = vec![0x00, 0x00, 0x00];
    p.extend_from_slice(&status.to_le_bytes());
    p.extend_from_slice(&0u16.to_le_bytes());
    p
}

/// An OK packet reporting `affected` changed rows (last_insert_id stays 0).
fn ok_packet_affected(status: u16, affected: u64) -> Vec<u8> {
    let mut p = vec![0x00];
    lenenc_int(&mut p, affected);
    lenenc_int(&mut p, 0);
    p.extend_from_slice(&status.to_le_bytes());
    p.extend_from_slice(&0u16.to_le_bytes());
    p
}

fn err_packet(code: u16, message: &str) -> Vec<u8> {
    let mut p = vec![0xff];
    p.extend_from_slice(&code.to_le_bytes());
    p.push(b'#');
    p.extend_from_slice(sqlstate(code).as_bytes());
    p.extend_from_slice(message.as_bytes());
    p
}

/// Builds an error packet, choosing the MySQL error number and SQLSTATE from
/// the engine's message text so drivers see meaningful codes.
fn error_packet(message: &str) -> Vec<u8> {
    err_packet(error_code(message), message)
}

/// Maps an engine error message to the closest MySQL error number.
fn error_code(message: &str) -> u16 {
    let m = message.to_ascii_lowercase();
    if m.contains("no such table") {
        1146
    } else if m.contains("no such column") {
        1054
    } else if m.contains("unknown function") {
        1305
    } else if m.contains("duplicate key") {
        1062
    } else if m.contains("database already exists") {
        1007
    } else if m.contains("already exists") {
        1050
    } else if m.contains("cannot be null") || m.contains("cannot have a null") {
        1048
    } else if m.contains("no such database") {
        1049
    } else if m.contains("permission denied") {
        1044
    } else if m.contains("not logged in")
        || m.contains("access denied")
        || m.contains("authentication failed")
    {
        1045
    } else if m.contains("could not serialize") {
        1213
    } else if m.contains("syntax") || m.contains("unexpected") || m.contains("expected") {
        1064
    } else {
        1105
    }
}

/// The SQLSTATE that conventionally accompanies a MySQL error number.
fn sqlstate(code: u16) -> &'static str {
    match code {
        1044 => "42000",
        1045 => "28000",
        1048 => "23000",
        1049 => "42000",
        1050 => "42S01",
        1054 => "42S22",
        1062 => "23000",
        1064 => "42000",
        1146 => "42S02",
        1305 => "42000",
        1213 => "40001",
        _ => "HY000",
    }
}

/// The isolation name MySQL clients expect from `@@transaction_isolation`.
fn isolation_label(isolation: Isolation) -> &'static str {
    match isolation {
        Isolation::ReadCommitted => "READ-COMMITTED",
        Isolation::RepeatableRead => "REPEATABLE-READ",
        Isolation::Serializable => "SERIALIZABLE",
    }
}

/// The OK/EOF status flags for the session's current autocommit mode.
fn session_status(session: &Session) -> u16 {
    if session.autocommit() { STATUS_AUTOCOMMIT } else { 0 }
}

/// Runs one COM_QUERY, honouring the client's `SET autocommit` and MySQL's
/// rule that DDL and database statements implicitly commit.
fn handle_query(
    instance: &crate::instance::Instance,
    stream: &mut TcpStream,
    session: &mut Session,
    payload: &[u8],
    conn_id: u32,
) -> io::Result<()> {
    let sql = String::from_utf8_lossy(payload);
    let isolation = isolation_label(instance.config().transaction.isolation);

    if let Some(autocommit) = compat::autocommit_setting(&sql) {
        if autocommit
            && let Err(e) = commit_if_open(instance, session)
        {
            return write_packet(stream, 1, &error_packet(&e.to_string()));
        }
        session.set_autocommit(autocommit);
        return write_packet(stream, 1, &ok_packet(session_status(session)));
    }

    if let Some(results) = compat::answer(&sql, session, conn_id, isolation) {
        return send_results(stream, &results, session_status(session));
    }

    // With autocommit off, a data statement opens a transaction that stays open
    // until COMMIT/ROLLBACK; DDL and database statements implicitly commit it.
    if let Ok(stmts) = crate::parser::parse(&sql)
        && !stmts.iter().any(is_transaction_control)
    {
        if stmts.iter().any(implicit_commit) {
            if let Err(e) = commit_if_open(instance, session) {
                return write_packet(stream, 1, &error_packet(&e.to_string()));
            }
        } else if !session.autocommit()
            && !session.in_transaction()
            && let Err(e) = instance.execute_with(session, "begin;")
        {
            return write_packet(stream, 1, &error_packet(&e.to_string()));
        }
    }

    match instance.execute_with(session, &sql) {
        Ok(results) => send_results(stream, &results, session_status(session)),
        Err(e) => write_packet(stream, 1, &error_packet(&e.to_string())),
    }
}

/// Commits the session's open transaction, if any (MySQL implicit commit).
fn commit_if_open(instance: &crate::instance::Instance, session: &mut Session) -> crate::Result<()> {
    if session.in_transaction() {
        instance.execute_with(session, "commit;")?;
    }
    Ok(())
}

fn is_transaction_control(stmt: &crate::ast::Stmt) -> bool {
    matches!(stmt, crate::ast::Stmt::Trx(_))
}

/// Statements that commit an open transaction before running (MySQL's implicit
/// commit): DDL, and everything routed at the instance/database level.
fn implicit_commit(stmt: &crate::ast::Stmt) -> bool {
    crate::is_exclusive(stmt)
        || matches!(
            stmt,
            crate::ast::Stmt::CreateDatabase(_)
                | crate::ast::Stmt::DropDatabase(_)
                | crate::ast::Stmt::Use(_)
                | crate::ast::Stmt::CreateUser(_)
                | crate::ast::Stmt::DropUser(_)
                | crate::ast::Stmt::Grant(_)
                | crate::ast::Stmt::Revoke(_)
                | crate::ast::Stmt::Login(_)
        )
}

fn eof_packet(status: u16) -> Vec<u8> {
    let mut p = vec![0xfe];
    p.extend_from_slice(&0u16.to_le_bytes());
    p.extend_from_slice(&status.to_le_bytes());
    p
}

fn column_definition(name: &str, ty: u8) -> Vec<u8> {
    // Numeric and date columns use the binary charset (63); text uses utf8.
    let charset: u16 = if ty == MYSQL_TYPE_VAR_STRING || ty == MYSQL_TYPE_STRING {
        0x21
    } else {
        0x3f
    };
    let mut p = Vec::new();
    for s in ["def", "", "", ""] {
        lenenc_str(&mut p, s);
    }
    lenenc_str(&mut p, name);
    lenenc_str(&mut p, "");
    p.push(0x0c);
    p.extend_from_slice(&charset.to_le_bytes());
    p.extend_from_slice(&255u32.to_le_bytes()); // display length
    p.push(ty);
    p.extend_from_slice(&0u16.to_le_bytes()); // flags
    p.push(0);
    p.extend_from_slice(&0u16.to_le_bytes());
    p
}

/// The MySQL type code for a value, inferred from the first non-null value of
/// its column (the text protocol sends the value as a string either way, but
/// clients use the type to decode it).
fn column_type_code(value: &Value) -> u8 {
    match value {
        Value::Int(_) => MYSQL_TYPE_LONGLONG,
        Value::Float(_) => MYSQL_TYPE_DOUBLE,
        Value::Bool(_) => MYSQL_TYPE_TINY,
        Value::Date(_) => MYSQL_TYPE_DATE,
        Value::Null | Value::Str(_) => MYSQL_TYPE_VAR_STRING,
    }
}

fn column_types(rows: &[Vec<Value>], count: usize) -> Vec<u8> {
    let mut types: Vec<Option<u8>> = vec![None; count];
    for row in rows {
        for (i, value) in row.iter().enumerate().take(count) {
            if types[i].is_none() && !matches!(value, Value::Null) {
                types[i] = Some(column_type_code(value));
            }
        }
    }
    types.into_iter().map(|t| t.unwrap_or(MYSQL_TYPE_VAR_STRING)).collect()
}

fn row_packet(row: &[Value]) -> Vec<u8> {
    let mut p = Vec::new();
    for value in row {
        match value {
            Value::Null => p.push(0xfb),
            other => lenenc_str(&mut p, &other.to_string()),
        }
    }
    p
}

fn lenenc_int(p: &mut Vec<u8>, n: u64) {
    if n < 251 {
        p.push(n as u8);
    } else if n < 1 << 16 {
        p.push(0xfc);
        p.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n < 1 << 24 {
        p.push(0xfd);
        p.extend_from_slice(&(n as u32).to_le_bytes()[0..3]);
    } else {
        p.push(0xfe);
        p.extend_from_slice(&n.to_le_bytes());
    }
}

fn lenenc_str(p: &mut Vec<u8>, s: &str) {
    lenenc_int(p, s.len() as u64);
    p.extend_from_slice(s.as_bytes());
}

fn write_packet(stream: &mut TcpStream, seq: u8, payload: &[u8]) -> io::Result<()> {
    let len = payload.len() as u32;
    stream.write_all(&[
        (len & 0xff) as u8,
        ((len >> 8) & 0xff) as u8,
        ((len >> 16) & 0xff) as u8,
        seq,
    ])?;
    stream.write_all(payload)?;
    stream.flush()
}

fn read_packet(stream: &mut TcpStream) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header)?;
    let len = header[0] as usize | (header[1] as usize) << 8 | (header[2] as usize) << 16;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok((header[3], payload))
}

fn read_u32(data: &[u8], pos: &mut usize) -> Option<u32> {
    let b = data.get(*pos..*pos + 4)?;
    *pos += 4;
    Some(u32::from_le_bytes(b.try_into().unwrap()))
}

fn read_nul_string(data: &[u8], pos: &mut usize) -> Option<String> {
    let bytes = read_nul_bytes(data, pos)?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_nul_bytes(data: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
    let start = *pos;
    while *pos < data.len() && data[*pos] != 0 {
        *pos += 1;
    }
    let bytes = data.get(start..*pos)?.to_vec();
    if *pos < data.len() {
        *pos += 1; // skip the terminator
    }
    Some(bytes)
}

/// SHA-1(SHA-1(password)) in lowercase hex: the `mysql_native_password`
/// verifier.
pub fn native_verifier_hex(password: &str) -> String {
    hex_encode(&sha1(&sha1(password.as_bytes())))
}

fn native_password_ok(verifier_hex: &str, scramble: &[u8], token: &[u8]) -> bool {
    let Some(verifier) = hex_decode(verifier_hex) else {
        return false;
    };
    if verifier.len() != 20 || token.len() != 20 {
        return false;
    }
    let mut input = Vec::with_capacity(scramble.len() + 20);
    input.extend_from_slice(scramble);
    input.extend_from_slice(&verifier);
    let mask = sha1(&input);
    let mut recovered = [0u8; 20];
    for i in 0..20 {
        recovered[i] = token[i] ^ mask[i];
    }
    sha1(&recovered)[..] == verifier[..]
}

fn random_scramble() -> [u8; 20] {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
    );
    hasher.write_u32(COUNTER.fetch_add(1, Ordering::Relaxed));
    let seed = hasher.finish().to_le_bytes();
    let mut material = Vec::with_capacity(16);
    material.extend_from_slice(&seed);
    material.extend_from_slice(&seed);
    sha1(&material)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

/// A compact SHA-1 (the native auth challenge is SHA-1 based).
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Connection housekeeping that MySQL clients send but the SQL dialect does
/// not implement. Answered locally so common clients (`mysql` CLI, pymysql,
/// ORMs) can connect without the engine growing protocol-only grammar.
mod compat {
    use crate::result::ResultSet;
    use crate::trx::Session;
    use crate::value::Value;

    /// The server version string reported to clients.
    pub(crate) const SERVER_VERSION: &str = "8.0.0-chibidb";

    /// Answers a housekeeping statement locally. `Some(vec![])` means "handled,
    /// reply with OK and no result set" (e.g. `SET ...`); `None` means the
    /// caller should run the statement through the engine.
    pub(crate) fn answer(
        input: &str,
        session: &Session,
        conn_id: u32,
        isolation: &str,
    ) -> Option<Vec<ResultSet>> {
        let sql = strip_comments(input).trim().trim_end_matches(';').trim();
        if sql.is_empty() {
            return Some(Vec::new());
        }
        let lower = sql.to_ascii_lowercase();
        if (starts_with_word(&lower, "commit") || starts_with_word(&lower, "rollback"))
            && !session.in_transaction()
        {
            // MySQL treats COMMIT/ROLLBACK with no transaction as a no-op.
            return Some(Vec::new());
        }
        if starts_with_word(&lower, "set") {
            // SET NAMES / autocommit / sql_mode / isolation are accepted and
            // ignored: the engine has no session variables to configure.
            return Some(Vec::new());
        }
        if starts_with_word(&lower, "show") {
            let rest = lower["show".len()..].trim_start();
            if rest.starts_with("warnings") || rest.starts_with("errors") {
                return Some(vec![ResultSet::Rows {
                    columns: vec!["Level".into(), "Code".into(), "Message".into()],
                    rows: Vec::new(),
                }]);
            }
            return None;
        }
        if !starts_with_word(&lower, "select") {
            return None;
        }
        select_variables(&sql["select".len()..], session, conn_id, isolation)
    }

    /// Recognizes `SET [SESSION|GLOBAL] [@@]autocommit = 0|1|ON|OFF|TRUE|FALSE`
    /// so the caller can track the client's autocommit mode. `None` for any
    /// other statement.
    pub(crate) fn autocommit_setting(input: &str) -> Option<bool> {
        let sql = strip_comments(input).trim().trim_end_matches(';').trim();
        let lower = sql.to_ascii_lowercase();
        let rest = lower.strip_prefix("set")?;
        if rest.starts_with(is_ident_char) {
            return None;
        }
        let rest = rest.trim_start();
        let rest = rest
            .strip_prefix("session")
            .or_else(|| rest.strip_prefix("global"))
            .or_else(|| rest.strip_prefix("local"))
            .unwrap_or(rest)
            .trim_start();
        let rest = rest.strip_prefix("@@").unwrap_or(rest);
        let rest = rest
            .strip_prefix("session.")
            .or_else(|| rest.strip_prefix("global."))
            .unwrap_or(rest);
        let rest = rest.strip_prefix("autocommit")?;
        let rest = rest.trim_start();
        let rest = rest.strip_prefix('=').unwrap_or(rest).trim_start();
        match rest {
            "1" | "on" | "true" => Some(true),
            "0" | "off" | "false" => Some(false),
            _ => None,
        }
    }

    /// Evaluates `SELECT <item>[, <item>...]`, requiring every item to be a
    /// system variable or a supported metadata function.
    fn select_variables(
        body: &str,
        session: &Session,
        conn_id: u32,
        isolation: &str,
    ) -> Option<Vec<ResultSet>> {
        let items = split_top_level(strip_limit(body), ',');
        let mut columns = Vec::with_capacity(items.len());
        let mut row = Vec::with_capacity(items.len());
        for item in items {
            let item = item.trim();
            let value = eval_item(item, session, conn_id, isolation)?;
            columns.push(item.to_string());
            row.push(value);
        }
        Some(vec![ResultSet::Rows { columns, rows: vec![row] }])
    }

    fn eval_item(item: &str, session: &Session, conn_id: u32, isolation: &str) -> Option<Value> {
        if let Some(value) = system_variable(item, isolation) {
            return Some(value);
        }
        function(item, session, conn_id)
    }

    /// `@@name` / `@@session.name` / `@@global.name`. Names that are not a
    /// single identifier (e.g. `@@v FROM t`) return `None` so the whole
    /// statement falls through to the engine instead of being misread.
    fn system_variable(item: &str, isolation: &str) -> Option<Value> {
        let rest = item.strip_prefix("@@")?;
        let name = parse_var_name(rest)?;
        Some(variable_value(&name, isolation))
    }

    fn parse_var_name(text: &str) -> Option<String> {
        let trimmed = text.trim();
        let unquoted = trimmed
            .strip_prefix('`')
            .and_then(|s| s.strip_suffix('`'))
            .unwrap_or(trimmed);
        let lower = unquoted.to_ascii_lowercase();
        let name = lower
            .strip_prefix("session.")
            .or_else(|| lower.strip_prefix("global."))
            .or_else(|| lower.strip_prefix("local."))
            .unwrap_or(lower.as_str());
        let valid = !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        valid.then(|| name.to_string())
    }

    fn variable_value(name: &str, isolation: &str) -> Value {
        match name {
            "version_comment" => Value::Str("chibidb".into()),
            "version" => Value::Str(SERVER_VERSION.into()),
            "sql_mode" => Value::Str(String::new()),
            "autocommit" => Value::Int(1),
            "character_set_client" | "character_set_connection" | "character_set_results"
            | "character_set_server" => Value::Str("utf8mb4".into()),
            "collation_connection" | "collation_server" | "collation_database" => {
                Value::Str("utf8mb4_general_ci".into())
            }
            "max_allowed_packet" => Value::Int(16 * 1024 * 1024),
            "net_buffer_length" => Value::Int(16 * 1024),
            "have_ssl" => Value::Str("DISABLED".into()),
            "have_query_cache" => Value::Str("NO".into()),
            "lower_case_table_names" => Value::Int(0),
            "time_zone" => Value::Str("SYSTEM".into()),
            "system_time_zone" => Value::Str("UTC".into()),
            "tx_isolation" | "transaction_isolation" => Value::Str(isolation.to_string()),
            "tx_read_only" | "transaction_read_only" => Value::Int(0),
            "wait_timeout" | "interactive_timeout" => Value::Int(28_800),
            "net_write_timeout" => Value::Int(60),
            "net_read_timeout" => Value::Int(30),
            "license" => Value::Str("GPL".into()),
            "init_connect" => Value::Str(String::new()),
            "foreign_key_checks" => Value::Int(1),
            "sql_auto_is_null" => Value::Int(0),
            "performance_schema" => Value::Int(0),
            "max_connections" => Value::Int(151),
            _ => Value::Str(String::new()),
        }
    }

    fn function(item: &str, session: &Session, conn_id: u32) -> Option<Value> {
        let normalized: String = item.to_ascii_lowercase().split_whitespace().collect();
        match normalized.as_str() {
            "version()" => Some(Value::Str(SERVER_VERSION.into())),
            "database()" | "schema()" => Some(
                session
                    .current_db()
                    .map(|d| Value::Str(d.to_string()))
                    .unwrap_or(Value::Null),
            ),
            "user()" | "current_user()" | "session_user()" | "system_user()" => {
                Some(Value::Str(session.user().unwrap_or("").to_string()))
            }
            "connection_id()" => Some(Value::Int(conn_id as i64)),
            "last_insert_id()" => Some(Value::Int(0)),
            _ => None,
        }
    }

    /// Drops leading `/* ... */` comments (clients prefix statements with them).
    fn strip_comments(mut s: &str) -> &str {
        loop {
            let t = s.trim_start();
            let Some(rest) = t.strip_prefix("/*") else {
                return t;
            };
            let Some(end) = rest.find("*/") else {
                return t;
            };
            s = &rest[end + 2..];
        }
    }

    fn starts_with_word(s: &str, word: &str) -> bool {
        match s.strip_prefix(word) {
            Some(rest) => !rest.starts_with(is_ident_char),
            None => false,
        }
    }

    fn is_ident_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_' || c == '$'
    }

    /// Cuts a trailing top-level `LIMIT ...` clause.
    fn strip_limit(body: &str) -> &str {
        let bytes = body.as_bytes();
        let mut depth = 0i32;
        let mut quote = 0u8;
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if quote != 0 {
                if c == quote {
                    quote = 0;
                }
                i += 1;
                continue;
            }
            match c {
                b'\'' | b'"' | b'`' => quote = c,
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            if depth == 0 && word_at(bytes, i, b"limit") {
                return body[..i].trim_end();
            }
            i += 1;
        }
        body
    }

    fn word_at(bytes: &[u8], i: usize, word: &[u8]) -> bool {
        if i + word.len() > bytes.len() || !bytes[i..i + word.len()].eq_ignore_ascii_case(word) {
            return false;
        }
        let before = i == 0 || !is_ident_char(bytes[i - 1] as char);
        let after = i + word.len();
        before && (after >= bytes.len() || !is_ident_char(bytes[after] as char))
    }

    /// Splits on top-level `sep`, ignoring quotes and parentheses.
    fn split_top_level(s: &str, sep: char) -> Vec<&str> {
        let mut out = Vec::new();
        let mut start = 0;
        let mut depth = 0i32;
        let mut quote = 0u8;
        for (i, c) in s.char_indices() {
            if quote != 0 {
                if c as u8 == quote {
                    quote = 0;
                }
                continue;
            }
            match c {
                '\'' | '"' | '`' => quote = c as u8,
                '(' => depth += 1,
                ')' => depth -= 1,
                _ if c == sep && depth == 0 => {
                    out.push(&s[start..i]);
                    start = i + c.len_utf8();
                }
                _ => {}
            }
        }
        out.push(&s[start..]);
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn answered(sql: &str) -> Option<Vec<ResultSet>> {
            answer(sql, &Session::new(), 7, "READ-COMMITTED")
        }

        fn one_value(sql: &str) -> Value {
            match answered(sql).unwrap().into_iter().next().unwrap() {
                ResultSet::Rows { mut rows, .. } => rows.remove(0).remove(0),
                other => panic!("expected rows, got {other:?}"),
            }
        }

        #[test]
        fn set_statements_are_accepted_and_ignored() {
            assert_eq!(answered("SET NAMES utf8mb4"), Some(Vec::new()));
            assert_eq!(answered("/* c */ set autocommit = 1"), Some(Vec::new()));
        }

        #[test]
        fn system_variables_are_answered() {
            assert_eq!(one_value("select @@version_comment limit 1"), Value::Str("chibidb".into()));
            assert_eq!(one_value("SELECT @@sql_mode"), Value::Str(String::new()));
            assert_eq!(
                one_value("select @@session.transaction_isolation"),
                Value::Str("READ-COMMITTED".into())
            );
        }

        #[test]
        fn metadata_functions_are_answered() {
            assert_eq!(
                one_value("SELECT VERSION()"),
                Value::Str(SERVER_VERSION.into())
            );
            assert_eq!(one_value("select connection_id()"), Value::Int(7));
        }

        #[test]
        fn engine_statements_fall_through() {
            assert_eq!(answered("select 1 as one"), None);
            assert_eq!(answered("select @@v from t"), None);
            assert_eq!(answered("insert into t values (1)"), None);
        }

        #[test]
        fn database_and_warnings() {
            let mut session = Session::new();
            session.set_current_db(Some("main".into()));
            match answer("select database()", &session, 1, "READ-COMMITTED")
                .unwrap()
                .into_iter()
                .next()
                .unwrap()
            {
                ResultSet::Rows { rows, .. } => {
                    assert_eq!(rows[0][0], Value::Str("main".into()));
                }
                other => panic!("expected rows, got {other:?}"),
            }
            match answer("show warnings", &session, 1, "READ-COMMITTED")
                .unwrap()
                .into_iter()
                .next()
                .unwrap()
            {
                ResultSet::Rows { rows, .. } => assert!(rows.is_empty()),
                other => panic!("expected empty rows, got {other:?}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_known_vectors() {
        assert_eq!(hex_encode(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex_encode(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn native_password_roundtrip() {
        let password = "secret";
        let verifier = native_verifier_hex(password);
        // the client computes sha1(password) then xors with sha1(scramble||verifier)
        let scramble = [7u8; 20];
        let password_sha = sha1(password.as_bytes());
        let mut input = scramble.to_vec();
        input.extend_from_slice(&hex_decode(&verifier).unwrap());
        let mask = sha1(&input);
        let mut token = [0u8; 20];
        for i in 0..20 {
            token[i] = password_sha[i] ^ mask[i];
        }
        assert!(native_password_ok(&verifier, &scramble, &token));
        assert!(!native_password_ok(&verifier, &scramble, &[0u8; 20]));
        assert!(!native_password_ok(&native_verifier_hex("other"), &scramble, &token));
    }

    #[test]
    fn length_encoded_integers_use_the_shortest_form() {
        let mut p = Vec::new();
        lenenc_int(&mut p, 5);
        assert_eq!(p, [5]);
        let mut p = Vec::new();
        lenenc_int(&mut p, 300);
        assert_eq!(p, [0xfc, 0x2c, 0x01]);
    }

    #[test]
    fn handshake_advertises_the_native_plugin() {
        let packet = handshake_packet(1, &[0u8; 20]);
        assert_eq!(packet[0], 10);
        assert!(packet.ends_with(b"mysql_native_password\0"));
    }
}
