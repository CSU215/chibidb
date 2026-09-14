//! Minimal HTTP/JSON frontend.
//!
//! Hand-rolled (no HTTP dependency): `POST /query` with a JSON body
//! `{"sql": "..."}` runs the statement batch and returns
//! `{"results": [...]}`; `GET /health` returns `{"status":"ok"}`. One session
//! is kept per connection so `LOGIN` and transactions span requests.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use tokio::net::TcpListener;

use crate::instance::Instance;
use crate::net::server::SharedInstance;
use crate::txn::trx::Session;

use super::admin::{self, Response};
use super::json::{encode_error, encode_results, json_string, json_string_field};
use super::session::{SESSION_HEADER, SessionRegistry, spawn_sweeper};

/// Largest request we will buffer (headers plus body).
const MAX_REQUEST: usize = 8 * 1024 * 1024;

/// Accepts HTTP connections until the listener closes.
pub async fn serve(instance: SharedInstance, listener: TcpListener) -> io::Result<()> {
    // One registry per listener. Sessions in it outlive the connections that
    // created them, so an abandoned one is rolled back on a timer rather than
    // when its socket closes.
    let registry = SessionRegistry::new();
    spawn_sweeper(Arc::clone(&registry), Arc::clone(&instance));
    loop {
        let (stream, peer) = listener.accept().await?;
        let stream = stream.into_std()?;
        stream.set_nonblocking(false)?;
        let instance = instance.clone();
        let registry = Arc::clone(&registry);
        std::thread::spawn(move || {
            if let Err(e) = serve_connection(&instance, stream, &registry) {
                eprintln!("http connection {peer} error: {e}");
            }
        });
    }
}

fn serve_connection(
    instance: &Instance,
    mut stream: TcpStream,
    registry: &SessionRegistry,
) -> io::Result<()> {
    // The session for requests that carry no header. Rolled back when the socket
    // goes away, exactly as it was before the registry existed.
    let mut local = Session::new();
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
                    &Response::json("413 Payload Too Large", encode_error("request too large")),
                    false,
                )?;
                break Ok(());
            }
        };
        read_body(&mut stream, &mut buf, body_end)?;

        let body_end = body_end.min(buf.len());
        let body = &buf[header_end..body_end];
        let (method, path, query) = request_line(&head);
        let target = Target { method: &method, path: &path, query: &query };
        let keep_alive = !head.to_ascii_lowercase().contains("connection: close");
        let response = dispatch(instance, registry, &mut local, &head, &target, body);
        write_response(&mut stream, &response, keep_alive)?;

        buf.drain(..body_end);
        if !keep_alive {
            break Ok(());
        }
    };
    // Only the connection's own session. A registry session is untouched, which
    // is the whole point: a browser closes pooled sockets at will.
    let _ = instance.rollback_session(&mut local);
    result
}

/// The parsed request line: method, path (without query) and query string.
struct Target<'a> {
    method: &'a str,
    path: &'a str,
    query: &'a str,
}

/// Picks the session for one request and runs it.
///
/// A request carrying `X-Chibi-Session` runs on that registry session; the id
/// comes back in the response, and is a new one when the requested id was
/// unknown. A request without the header runs on the connection's own session
/// and gets no id, which is what keeps stateless clients identical to before.
fn dispatch(
    instance: &Instance,
    registry: &SessionRegistry,
    local: &mut Session,
    head: &str,
    target: &Target<'_>,
    body: &[u8],
) -> Response {
    // `GET /session` mints an id without running a statement, so a fresh page
    // can pick one up before its first query. It lives here rather than in the
    // admin module because sessions are this module's business, and because it
    // must work whether or not `admin_api` is on.
    if target.method == "GET" && target.path == "/session" {
        let (id, _session) = registry.mint();
        let mut response =
            Response::json("200 OK", format!("{{\"session\":{}}}", json_string(&id)));
        response.extra_headers.push((SESSION_HEADER, id));
        return response;
    }

    let Some(requested) = header(head, SESSION_HEADER) else {
        return route(instance, local, target, body);
    };
    let (id, session) = registry.claim(requested);
    // Locking after `claim` returned, never while holding the registry lock.
    let mut response = route(instance, &mut session.lock(), target, body);
    response.extra_headers.push((SESSION_HEADER, id));
    response
}

fn route(
    instance: &Instance,
    session: &mut Session,
    target: &Target<'_>,
    body: &[u8],
) -> Response {
    // The admin surface owns `/api/*` and static hosting; it declines the two
    // legacy paths so they keep their exact previous behaviour.
    if let Some(response) =
        admin::handle(instance, session, target.method, target.path, target.query, body)
    {
        return response;
    }
    match (target.method, target.path) {
        ("GET", "/health") => Response::json("200 OK", "{\"status\":\"ok\"}".to_string()),
        ("POST", "/query") => {
            let text = String::from_utf8_lossy(body);
            let Some(sql) = json_string_field(&text, "sql") else {
                return Response::json(
                    "400 Bad Request",
                    encode_error("expected a JSON body {\"sql\": ...}"),
                );
            };
            match instance.execute_with(session, &sql) {
                Ok(results) => Response::json("200 OK", encode_results(&results)),
                Err(e) => Response::json("400 Bad Request", encode_error(&e.to_string())),
            }
        }
        _ => Response::not_found(),
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

fn request_line(head: &str) -> (String, String, String) {
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/");
    // The fragment is never sent by a browser, but drop it anyway. `/assets/app.js?v=1`
    // names the file `app.js`, with `v=1` as the query.
    let target = target.split('#').next().unwrap_or("/");
    match target.split_once('?') {
        Some((path, query)) => (method, path.to_string(), query.to_string()),
        None => (method, target.to_string(), String::new()),
    }
}

/// The first value for a header, case-insensitively. The request line has no
/// colon, so scanning every line including it is safe.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

fn content_length(head: &str) -> Option<usize> {
    header(head, "content-length")?.parse().ok()
}

/// Writes one response. The body is bytes, not a string: static assets include
/// binaries (`woff2`, `png`) that a lossy UTF-8 round trip would corrupt.
fn write_response(stream: &mut TcpStream, response: &Response, keep_alive: bool) -> io::Result<()> {
    let connection = if keep_alive { "keep-alive" } else { "close" };
    let mut header = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {connection}\r\n",
        response.status,
        response.content_type,
        response.body.len(),
    );
    for (name, value) in &response.extra_headers {
        header.push_str(name);
        header.push_str(": ");
        header.push_str(value);
        header.push_str("\r\n");
    }
    header.push_str("\r\n");
    stream.write_all(header.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
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
    use crate::sql::result::ResultSet;
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
