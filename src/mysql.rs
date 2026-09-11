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
    write_packet(&mut stream, 2, &ok_packet(STATUS_AUTOCOMMIT))?;
    if let Some(db) = &hs.database {
        let _ = instance.execute_with(&mut session, &format!("use {db};"));
    }

    let mut prepared: HashMap<u32, String> = HashMap::new();
    let mut next_statement_id: u32 = 1;

    while let Ok((_, packet)) = read_packet(&mut stream) {
        let Some((&command, payload)) = packet.split_first() else {
            continue;
        };
        match command {
            COM_QUIT => break,
            COM_PING => write_packet(&mut stream, 1, &ok_packet(STATUS_AUTOCOMMIT))?,
            COM_INIT_DB => {
                let db = String::from_utf8_lossy(payload);
                let _ = instance.execute_with(&mut session, &format!("use {db};"));
                write_packet(&mut stream, 1, &ok_packet(STATUS_AUTOCOMMIT))?;
            }
            COM_QUERY => {
                let sql = String::from_utf8_lossy(payload);
                match instance.execute_with(&mut session, &sql) {
                    Ok(results) => send_results(&mut stream, &results)?,
                    Err(e) => write_packet(&mut stream, 1, &err_packet(1064, &e.to_string()))?,
                }
            }
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
                    write_packet(&mut stream, seq, &column_definition("?"))?;
                    seq = seq.wrapping_add(1);
                }
                if params > 0 {
                    write_packet(&mut stream, seq, &eof_packet(STATUS_AUTOCOMMIT))?;
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
                    Ok(results) => send_results(&mut stream, &results)?,
                    Err(e) => write_packet(&mut stream, 1, &err_packet(1064, &e.to_string()))?,
                }
            }
            COM_STMT_CLOSE => {
                let mut pos = 0;
                if let Some(id) = read_u32(payload, &mut pos) {
                    prepared.remove(&id);
                }
            }
            COM_STMT_RESET => write_packet(&mut stream, 1, &ok_packet(STATUS_AUTOCOMMIT))?,
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

fn send_results(stream: &mut TcpStream, results: &[ResultSet]) -> io::Result<()> {
    let mut seq = 1u8;
    for (i, result) in results.iter().enumerate() {
        let more = i + 1 < results.len();
        let status = STATUS_AUTOCOMMIT | if more { STATUS_MORE_RESULTS } else { 0 };
        match result {
            ResultSet::Message(_) => {
                write_packet(stream, seq, &ok_packet(status))?;
                seq = seq.wrapping_add(1);
            }
            ResultSet::Rows { columns, rows } => {
                let mut count = Vec::new();
                lenenc_int(&mut count, columns.len() as u64);
                write_packet(stream, seq, &count)?;
                seq = seq.wrapping_add(1);
                for column in columns {
                    write_packet(stream, seq, &column_definition(column))?;
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
    let bytes = data.get(pos..pos + len)?.to_vec();
    Some((bytes, pos + len))
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

fn err_packet(code: u16, message: &str) -> Vec<u8> {
    let mut p = vec![0xff];
    p.extend_from_slice(&code.to_le_bytes());
    p.push(b'#');
    p.extend_from_slice(b"HY000");
    p.extend_from_slice(message.as_bytes());
    p
}

fn eof_packet(status: u16) -> Vec<u8> {
    let mut p = vec![0xfe];
    p.extend_from_slice(&0u16.to_le_bytes());
    p.extend_from_slice(&status.to_le_bytes());
    p
}

fn column_definition(name: &str) -> Vec<u8> {
    let mut p = Vec::new();
    for s in ["def", "", "", ""] {
        lenenc_str(&mut p, s);
    }
    lenenc_str(&mut p, name);
    lenenc_str(&mut p, "");
    p.push(0x0c);
    p.extend_from_slice(&0x21u16.to_le_bytes()); // charset
    p.extend_from_slice(&255u32.to_le_bytes()); // display length
    p.push(0xfd); // MYSQL_TYPE_VAR_STRING
    p.extend_from_slice(&0u16.to_le_bytes()); // flags
    p.push(0);
    p.extend_from_slice(&0u16.to_le_bytes());
    p
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
