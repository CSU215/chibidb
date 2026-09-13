//! Minimal HTTP/JSON frontend.
//!
//! Hand-rolled (no HTTP dependency): `POST /query` with a JSON body
//! `{"sql": "..."}` runs the statement batch and returns
//! `{"results": [...]}`; `GET /health` returns `{"status":"ok"}`. One session
//! is kept per connection so `LOGIN` and transactions span requests.

use std::io::{self, Read, Write};
use std::net::TcpStream;

use tokio::net::TcpListener;

use crate::instance::Instance;
use crate::result::{encode_error, encode_results};
use crate::trx::Session;

use crate::server::SharedInstance;

/// Largest request we will buffer (headers plus body).
const MAX_REQUEST: usize = 8 * 1024 * 1024;

/// Accepts HTTP connections until the listener closes.
pub async fn serve(instance: SharedInstance, listener: TcpListener) -> io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let stream = stream.into_std()?;
        stream.set_nonblocking(false)?;
        let instance = instance.clone();
        std::thread::spawn(move || {
            if let Err(e) = serve_connection(&instance, stream) {
                eprintln!("http connection {peer} error: {e}");
            }
        });
    }
}

fn serve_connection(instance: &Instance, mut stream: TcpStream) -> io::Result<()> {
    let mut session = Session::new();
    let mut buf: Vec<u8> = Vec::new();
    let result = loop {
        let Some(header_end) = read_headers(&mut stream, &mut buf)? else {
            break Ok(());
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let content_length = content_length(&head).unwrap_or(0);
        // Guard against an overflow or an oversized body from a hostile header
        // before allocating/buffering anything.
        let body_end = match header_end.checked_add(content_length) {
            Some(end) if end <= MAX_REQUEST => end,
            _ => {
                write_response(
                    &mut stream,
                    "413 Payload Too Large",
                    &encode_error("request too large"),
                    false,
                )?;
                break Ok(());
            }
        };
        read_body(&mut stream, &mut buf, body_end)?;

        let body_end = body_end.min(buf.len());
        let body = &buf[header_end..body_end];
        let (method, path) = request_line(&head);
        let keep_alive = !head.to_ascii_lowercase().contains("connection: close");
        let (status, payload) = route(instance, &mut session, &method, &path, body);
        write_response(&mut stream, status, &payload, keep_alive)?;

        buf.drain(..body_end);
        if !keep_alive {
            break Ok(());
        }
    };
    let _ = instance.rollback_session(&mut session);
    result
}

fn route(
    instance: &Instance,
    session: &mut Session,
    method: &str,
    path: &str,
    body: &[u8],
) -> (&'static str, String) {
    match (method, path) {
        ("GET", "/health") => ("200 OK", "{\"status\":\"ok\"}".to_string()),
        ("POST", "/query") => {
            let text = String::from_utf8_lossy(body);
            let Some(sql) = json_string_field(&text, "sql") else {
                return ("400 Bad Request", encode_error("expected a JSON body {\"sql\": ...}"));
            };
            match instance.execute_with(session, &sql) {
                Ok(results) => ("200 OK", encode_results(&results)),
                Err(e) => ("400 Bad Request", encode_error(&e.to_string())),
            }
        }
        _ => ("404 Not Found", encode_error("not found")),
    }
}

/// Reads until the header terminator; returns its end offset, or `None` on EOF.
fn read_headers(stream: &mut TcpStream, buf: &mut Vec<u8>) -> io::Result<Option<usize>> {
    loop {
        if let Some(pos) = find(buf, b"\r\n\r\n") {
            return Ok(Some(pos + 4));
        }
        if buf.len() > MAX_REQUEST {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request too large"));
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn read_body(stream: &mut TcpStream, buf: &mut Vec<u8>, want: usize) -> io::Result<()> {
    while buf.len() < want {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(())
}

fn request_line(head: &str) -> (String, String) {
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    (method, path)
}

fn content_length(head: &str) -> Option<usize> {
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    body: &str,
    keep_alive: bool,
) -> io::Result<()> {
    let connection = if keep_alive { "keep-alive" } else { "close" };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// Extracts a top-level string field from a tiny JSON object.
fn json_string_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)? + needle.len();
    let rest = text[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'u' => {
                    let mut hex = String::new();
                    for _ in 0..4 {
                        hex.push(chars.next()?);
                    }
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                _ => return None,
            },
            c => out.push(c),
        }
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::ResultSet;
    use crate::value::Value;

    #[test]
    fn encodes_rows_and_messages() {
        let results = vec![
            ResultSet::Message("SUCCESS".into()),
            ResultSet::Rows {
                columns: vec!["id".into(), "name".into()],
                rows: vec![vec![Value::Int(1), Value::Str("a\"b".into())]],
            },
        ];
        let json = encode_results(&results);
        assert!(json.contains("\"type\":\"message\""));
        assert!(json.contains("\"type\":\"rows\""));
        assert!(json.contains("[\"id\",\"name\"]"));
        assert!(json.contains("a\\\"b"));
    }

    #[test]
    fn parses_a_sql_field_with_escapes() {
        let body = r#"{"sql": "select \"x\" from t;\n"}"#;
        assert_eq!(json_string_field(body, "sql").unwrap(), "select \"x\" from t;\n");
        assert!(json_string_field("{}", "sql").is_none());
    }
}
