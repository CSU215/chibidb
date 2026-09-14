//! Static hosting for the built SPA, plus the `/api/*` surface.
//!
//! `http.rs` owns HTTP framing; this module owns the routes and their content.
//! [`handle`] returns `None` for the paths the legacy frontend already serves
//! (`/health` and `/query`), so those keep working untouched.

use std::path::Path;

use crate::config::{Config, WebRootState};
use crate::instance::Instance;
use crate::introspect;
use crate::lexer::{Punct, Token, TokenKind};
use crate::net::json::{self, Json};
use crate::result::{encode_error, json_string};
use crate::trx::Session;
use crate::{Error, lexer, parser};

/// A response for `http.rs` to frame and write out.
pub(crate) struct Response {
    pub status: &'static str,
    pub content_type: &'static str,
    pub extra_headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    fn new(status: &'static str, content_type: &'static str, body: Vec<u8>) -> Self {
        Self { status, content_type, extra_headers: Vec::new(), body }
    }

    pub(crate) fn json(status: &'static str, body: String) -> Self {
        Self::new(status, "application/json", body.into_bytes())
    }

    pub(crate) fn not_found() -> Self {
        Self::json("404 Not Found", encode_error("not found"))
    }
}

/// Routes the paths this module owns. `None` hands the request back to
/// `http.rs`, which serves `/health` and `/query`.
///
/// The instance and the session are threaded through because an endpoint that
/// inspects the engine (`/api/plan`) needs the statement's database, resolved
/// for that session exactly as a query would resolve it.
pub(crate) fn handle(
    instance: &Instance,
    session: &mut Session,
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
) -> Option<Response> {
    match (method, path) {
        // The legacy frontend keeps these two.
        ("GET", "/health") | ("POST", "/query") => None,
        (_, path) if path.starts_with("/api/") => {
            Some(api(instance, session, method, path, query, body))
        }
        ("GET", path) => Some(static_file(instance.config(), path)),
        _ => None,
    }
}

/// The `/api/*` surface. Off unless `server.admin_api` says otherwise, so the
/// whole namespace answers 404 on a default deployment.
fn api(
    instance: &Instance,
    session: &mut Session,
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
) -> Response {
    if !instance.config().server.admin_api {
        return Response::not_found();
    }
    match (method, path) {
        ("POST", "/api/parse") => parse_trace(body),
        ("POST", "/api/plan") => plan_trace(instance, session, body),
        ("GET", "/api/metrics") => metrics_trace(instance, session),
        ("GET", "/api/bufferpool/frames") => frames_trace(instance, session),
        ("GET", "/api/bufferpool/events") => events_trace(instance, session, query),
        _ => Response::not_found(),
    }
}

/// `GET /api/metrics` -- cumulative pool counters, the WAL's position, and one
/// line per LSM table.
///
/// **Cumulative, not a rate.** The console samples this on a timer and
/// subtracts; the engine keeps no window, so changing the polling interval
/// changes nothing on this side. Same rule as `tests/perf_stats.rs`.
fn metrics_trace(instance: &Instance, session: &mut Session) -> Response {
    with_current_db(instance, session, |database, db| {
        Response::json("200 OK", introspect::pool::metrics(database, db).to_json())
    })
}

/// `GET /api/bufferpool/frames` -- what is resident right now, with the pin
/// count and dirty bit each frame is in.
fn frames_trace(instance: &Instance, session: &mut Session) -> Response {
    with_current_db(instance, session, |_, db| {
        Response::json("200 OK", introspect::pool::frames_json(&introspect::pool::frames(db)))
    })
}

/// `GET /api/bufferpool/events?since=N` -- the event log from a cursor.
///
/// The cursor is the `seq` of the last event the caller saw; the response's
/// `next` is what to send back. `truncated` means the caller was away longer
/// than the ring is deep, so it must start over rather than believe it saw
/// everything.
fn events_trace(instance: &Instance, session: &mut Session, query: &str) -> Response {
    let since = match query_param(query, "since") {
        None => 0,
        Some(text) => match text.parse::<u64>() {
            Ok(seq) => seq,
            Err(_) => {
                return Response::json(
                    "400 Bad Request",
                    encode_error("since must be a non-negative integer"),
                );
            }
        },
    };
    with_current_db(instance, session, |_, db| {
        Response::json(
            "200 OK",
            introspect::pool::events_json(&introspect::pool::events(db, since)),
        )
    })
}

/// Runs `f` against the session's database, resolved exactly the way a query
/// would resolve it (including creating and selecting the default one), and
/// held under the shared lock: these endpoints only read.
fn with_current_db<F>(instance: &Instance, session: &mut Session, f: F) -> Response
where
    F: FnOnce(&str, &crate::Database) -> Response,
{
    let database = match instance.ensure_current_db(session) {
        Ok(name) => name,
        Err(e) => return Response::json("500 Internal Server Error", encode_error(&e.to_string())),
    };
    let Ok(db) = instance.database(&database) else {
        return Response::json(
            "500 Internal Server Error",
            encode_error("database is not open"),
        );
    };
    f(&database, &db.read())
}

/// One parameter out of a query string, percent-decoding left out on purpose:
/// the only parameter in use is a decimal cursor, and anything that would need
/// decoding is rejected rather than guessed at.
fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

/// `POST /api/plan` -- the plan the engine will run, why it chose it, and what
/// the names resolved to.
///
/// **The status is 200 even when the statement does not parse or has no plan.**
/// Like `/api/parse`, this endpoint diagnoses text as it is being written, so a
/// statement with nothing to show is a result rather than a transport error;
/// callers read `error` and `plans[].error`. A 400 means the *request* was
/// malformed (not JSON, or no usable `sql` field).
///
/// The tree it returns is built by the executor's own `build_statement` and
/// then dropped unopened, so it is the tree that would run -- not a rendering
/// of `exec/plan.rs`, which is a parallel implementation of the same choice
/// (that one is reported separately, under `explain`/`chosen`/`rejected`).
fn plan_trace(instance: &Instance, session: &mut Session, body: &[u8]) -> Response {
    let text = String::from_utf8_lossy(body);
    let Ok(request) = json::parse(&text) else {
        return Response::json("400 Bad Request", encode_error("expected a JSON object"));
    };
    let Some(sql) = request.get("sql").and_then(Json::as_str) else {
        return Response::json(
            "400 Bad Request",
            encode_error("expected a string field \"sql\""),
        );
    };

    let database = match instance.ensure_current_db(session) {
        Ok(name) => name,
        Err(e) => return plan_error(sql, "", &e.to_string()),
    };
    let Ok(db) = instance.database(&database) else {
        return plan_error(sql, &database, "database is not open");
    };

    // Lex first so a failure can be placed: the lexer is all-or-nothing, so a
    // lex error means there is no token stream, while a parse error leaves one
    // worth showing. Same three-way outcome as `/api/parse`.
    let (mut statements, mut error) = match lexer::lex(sql) {
        Ok(_) => (Vec::new(), None),
        Err(e) => (Vec::new(), Some(("lex", e))),
    };
    if error.is_none() {
        match parser::parse(sql) {
            Ok(parsed) => statements = parsed,
            Err(e) => error = Some(("parse", e)),
        }
    }

    let guard = db.read();
    let plans: Vec<String> = statements
        .iter()
        .map(|stmt| introspect::plan::report(&database, &guard, stmt).to_json())
        .collect();

    let mut out = String::from("{\"sql\":");
    out.push_str(&json_string(sql));
    out.push_str(",\"database\":");
    out.push_str(&json_string(&database));
    out.push_str(",\"error\":");
    match &error {
        None => out.push_str("null"),
        Some((stage, error)) => {
            let (message, pos) = syntax_parts(error);
            out.push_str(&format!(
                "{{\"stage\":{},\"message\":{},\"pos\":{}}}",
                json_string(stage),
                json_string(&message),
                pos.map_or_else(|| "null".to_string(), |at| at.to_string()),
            ));
        }
    }
    out.push_str(",\"plans\":[");
    out.push_str(&join(plans.into_iter()));
    out.push_str("]}");
    Response::json("200 OK", out)
}

/// A plan response that could not even get as far as a statement.
fn plan_error(sql: &str, database: &str, message: &str) -> Response {
    Response::json(
        "200 OK",
        format!(
            "{{\"sql\":{},\"database\":{},\"error\":{{\"stage\":\"database\",\"message\":{}}},\"plans\":[]}}",
            json_string(sql),
            json_string(database),
            json_string(message),
        ),
    )
}

/// `POST /api/parse` -- the SQL as the compiler sees it, without running it.
///
/// **The status is 200 even when the SQL does not parse.** This endpoint
/// diagnoses text the user is still typing, so a parse failure is a result
/// rather than a transport error; callers read `error`, not the status. A 400
/// means the *request* was malformed (not JSON, or no usable `sql` field).
///
/// There are three outcomes, because the lexer is all-or-nothing: `error` null,
/// `error.stage == "lex"` (no token stream), or `error.stage == "parse"` (the
/// token stream is still worth showing).
fn parse_trace(body: &[u8]) -> Response {
    let text = String::from_utf8_lossy(body);
    let Ok(request) = json::parse(&text) else {
        return Response::json("400 Bad Request", encode_error("expected a JSON object"));
    };
    let Some(sql) = request.get("sql").and_then(Json::as_str) else {
        return Response::json(
            "400 Bad Request",
            encode_error("expected a string field \"sql\""),
        );
    };

    let (tokens, mut error) = match lexer::lex(sql) {
        Ok(tokens) => (tokens, None),
        Err(e) => (Vec::new(), Some(("lex", e))),
    };
    let mut statements = Vec::new();
    if error.is_none() {
        match parser::parse(sql) {
            Ok(parsed) => statements = parsed.iter().map(|stmt| format!("{stmt:#?}")).collect(),
            Err(e) => error = Some(("parse", e)),
        }
    }

    let mut out = String::from("{\"sql\":");
    out.push_str(&json_string(sql));
    out.push_str(",\"tokens\":[");
    out.push_str(&join(tokens.iter().map(token_json)));
    out.push_str("],\"statements\":[");
    out.push_str(&join(statements.iter().map(|debug| {
        format!("{{\"debug\":{}}}", json_string(debug))
    })));
    out.push_str("],\"error\":");
    match error {
        None => out.push_str("null"),
        Some((stage, error)) => {
            let (message, pos) = syntax_parts(&error);
            out.push_str(&format!(
                "{{\"stage\":{},\"message\":{},\"pos\":{}}}",
                json_string(stage),
                json_string(&message),
                // Null rather than 0: byte 0 is a real position, and a client
                // must be able to tell "no position" from "the very first byte".
                pos.map_or_else(|| "null".to_string(), |at| at.to_string()),
            ));
        }
    }
    out.push('}');
    Response::json("200 OK", out)
}

/// The message and byte offset of a compiler complaint.
///
/// The offset is only available for syntax errors: once execution starts there
/// is no token to point at, so a runtime complaint (`no such column`) carries
/// no position and the client falls back to showing it in the banner.
fn syntax_parts(error: &Error) -> (String, Option<usize>) {
    match error {
        Error::Syntax { message, pos } => (format!("syntax error: {message}"), *pos),
        other => (other.to_string(), None),
    }
}

fn join(items: impl Iterator<Item = String>) -> String {
    items.collect::<Vec<_>>().join(",")
}

/// One lexer token. `text` is the lexeme as written; for a number it is the
/// original spelling, so it stays a string and cannot be mistaken for a value.
fn token_json(token: &Token) -> String {
    let (kind, text) = match &token.kind {
        TokenKind::Int(n) => ("Int", n.to_string()),
        TokenKind::Float(x) => ("Float", x.to_string()),
        TokenKind::Str(s) => ("Str", s.clone()),
        TokenKind::Ident(s) => ("Ident", s.clone()),
        TokenKind::Punct(p) => ("Punct", punct_text(*p).to_string()),
    };
    format!(
        "{{\"kind\":{},\"text\":{},\"pos\":{}}}",
        json_string(kind),
        json_string(&text),
        token.pos
    )
}

fn punct_text(punct: Punct) -> &'static str {
    match punct {
        Punct::LParen => "(",
        Punct::RParen => ")",
        Punct::Comma => ",",
        Punct::Semicolon => ";",
        Punct::Plus => "+",
        Punct::Minus => "-",
        Punct::Star => "*",
        Punct::Slash => "/",
        Punct::Percent => "%",
        Punct::Eq => "=",
        Punct::NotEq => "!=",
        Punct::Lt => "<",
        Punct::Le => "<=",
        Punct::Gt => ">",
        Punct::Ge => ">=",
        Punct::Dot => ".",
    }
}

/// Serves one file from the configured web root.
fn static_file(config: &Config, path: &str) -> Response {
    // No percent-decoding: anything that would need it is rejected instead. Vite
    // emits ASCII with hashed names, so nothing legitimate is lost, and `%2e%2e`
    // cannot be smuggled through.
    if !path.is_ascii() || path.contains('%') {
        return Response::not_found();
    }
    let relative = path.trim_start_matches('/');
    let is_index = relative.is_empty();
    let relative = if is_index { "index.html" } else { relative };
    // Reject traversal before touching the filesystem.
    if relative.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..") {
        return Response::not_found();
    }
    let base = match config.web_root_state() {
        WebRootState::Ready(base) => base,
        // A fresh clone has no built SPA. Answering a bare 404 for `/` reads as
        // "the server is broken"; say what to run instead.
        WebRootState::Missing(looked_in) if is_index => return missing_web_root_page(&looked_in),
        WebRootState::Missing(_) | WebRootState::Off => return Response::not_found(),
    };
    let Ok(resolved) = base.join(relative).canonicalize() else {
        return Response::not_found();
    };
    // `Path::starts_with` compares whole components; `str::starts_with` would
    // wrongly accept `/srv/web/dist-evil` for a root of `/srv/web/dist`.
    if !resolved.starts_with(&base) {
        return Response::not_found();
    }
    let Ok(body) = std::fs::read(&resolved) else {
        return Response::not_found();
    };
    let mut response = Response::new("200 OK", content_type(&resolved), body);
    // Vite puts hashed assets under `assets/`; everything else is an entry
    // document that must be revalidated after a rebuild.
    let cache = if relative.starts_with("assets/") {
        "max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    response.extra_headers.push(("Cache-Control", cache.to_string()));
    response.extra_headers.push(("X-Content-Type-Options", "nosniff".to_string()));
    response
}

/// The page served for `/` when the web root is configured but absent.
fn missing_web_root_page(looked_in: &str) -> Response {
    let origin = "POST /query {\"sql\":\"...\"}";
    let body = format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<title>chibidb</title>\n\
         <h1>chibidb</h1>\n\
         <p>The web console is not built. Expected it at\n<code>{}</code>.</p>\n\
         <p>Build it with <code>scripts/build_web.sh</code> (needs Node), or run the\n\
         dev server with <code>cd web &amp;&amp; npm run dev</code>.</p>\n\
         <p>The SQL endpoints work either way: <code>GET /health</code>,\n\
         <code>{origin}</code>.</p>\n",
        escape_html(looked_in)
    );
    let mut response = Response::new("200 OK", "text/html; charset=utf-8", body.into_bytes());
    response.extra_headers.push(("Cache-Control", "no-cache".to_string()));
    response
}

/// Escapes the five HTML metacharacters. The path comes from the operator's own
/// config, not from a request, but an unescaped `&` would still produce broken
/// markup and hide the very message that is meant to help.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The content type for a served file, by extension.
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
