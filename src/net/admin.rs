//! Static hosting for the built SPA, plus the `/api/*` surface.
//!
//! `http.rs` owns HTTP framing; this module owns the routes and their content.
//! [`handle`] returns `None` for the paths the legacy frontend already serves
//! (`/health` and `/query`), so those keep working untouched.

use std::path::Path;

use crate::catalog::meta::TableMeta;
use crate::config::{Config, EngineKind, PageLayout, WebRootState};
use crate::exec::operator;
use crate::instance::Instance;
use crate::net::json::{self, encode_error, json_string, Json};
use crate::sql::ast::Stmt;
use crate::sql::lexer::{Punct, Token, TokenKind};
use crate::sql::{lexer, parser};
use crate::txn::trx::Session;
use crate::Error;

use super::inspect;

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
pub(crate) fn handle(
    instance: &Instance,
    session: &mut Session,
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
) -> Option<Response> {
    let config = instance.config();
    match (method, path) {
        // The legacy frontend keeps these two.
        ("GET", "/health") | ("POST", "/query") => None,
        (_, path) if path.starts_with("/api/") => {
            Some(api(instance, session, method, path, query, body))
        }
        ("GET", path) => {
            // The embedded demo console wins over a configured web root.
            if config.web.enabled && let Some(asset) = embedded_asset(path) {
                Some(asset)
            } else {
                Some(static_file(config, path))
            }
        }
        _ => None,
    }
}

/// The `/api/*` surface. Off unless `server.admin_api` says otherwise, so the
/// whole namespace answers 404 on a default deployment.
fn api(
    instance: &Instance,
    session: &Session,
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
) -> Response {
    let config = instance.config();
    if !config.server.admin_api {
        return Response::not_found();
    }
    match (method, path) {
        ("POST", "/api/parse") => parse_trace(body),
        ("POST", "/api/plan") => plan_trace(instance, session, body),
        ("GET", "/api/config") => config_trace(config),
        ("GET", "/api/schema") => schema_trace(instance),
        ("GET", "/api/files") => files_trace(instance, config, query),
        ("GET", "/api/page") => inspect::page(instance, config, query),
        _ => Response::not_found(),
    }
}

// ---------------------------------------------------------------- /api/parse

/// `POST /api/parse` -- the SQL as the compiler sees it, without running it.
///
/// **The status is 200 even when the SQL does not parse.** This endpoint
/// diagnoses text the user is still typing, so a parse failure is a result
/// rather than a transport error; callers read `error`, not the status. A 400
/// means the *request* was malformed (not JSON, or no usable `sql` field).
fn parse_trace(body: &[u8]) -> Response {
    let Some(sql) = request_sql(body) else {
        return bad_request();
    };

    let (tokens, statements, error) = compile(&sql);

    let mut out = String::from("{\"sql\":");
    out.push_str(&json_string(&sql));
    out.push_str(",\"tokens\":[");
    out.push_str(&tokens_json(&tokens));
    out.push_str("],\"statements\":[");
    out.push_str(&statements_json(&statements));
    out.push_str("],\"error\":");
    out.push_str(&error_json(error.as_ref()));
    out.push('}');
    Response::json("200 OK", out)
}

// ----------------------------------------------------------------- /api/plan

/// `POST /api/plan` -- the lexed, parsed and planned form of the statement.
///
/// Like `/api/parse`, a compile failure is a 200 with `error` populated; a 400
/// means the request body itself was malformed.
fn plan_trace(instance: &Instance, session: &Session, body: &[u8]) -> Response {
    let Some(sql) = request_sql(body) else {
        return bad_request();
    };

    let (tokens, statements, mut error) = compile(&sql);

    let mut plan = None;
    let mut physical = None;
    if error.is_none()
        && let Ok(statements) = parser::parse(&sql)
        && let Some(stmt) = statements.first()
    {
        match resolve_statement(instance, session, stmt) {
            Ok((stmt_plan, stmt_physical)) => {
                plan = stmt_plan;
                physical = stmt_physical;
            }
            Err(e) => error = Some(("plan", e)),
        }
    }

    let mut out = String::from("{\"sql\":");
    out.push_str(&json_string(&sql));
    out.push_str(",\"tokens\":[");
    out.push_str(&tokens_json(&tokens));
    out.push_str("],\"statements\":[");
    out.push_str(&statements_json(&statements));
    out.push_str("],\"plan\":");
    out.push_str(&string_or_null(plan.as_deref()));
    out.push_str(",\"physical\":");
    out.push_str(&string_or_null(physical.as_deref()));
    out.push_str(",\"error\":");
    out.push_str(&error_json(error.as_ref()));
    out.push('}');
    Response::json("200 OK", out)
}

/// Plans the first statement against its database. Only a `SELECT` produces
/// text this endpoint reports; other statements plan to `None`.
fn resolve_statement(
    instance: &Instance,
    session: &Session,
    stmt: &Stmt,
) -> std::result::Result<(Option<String>, Option<String>), Error> {
    let db_name = session.current_db().unwrap_or(crate::instance::DEFAULT_DB);
    let db = instance.database(db_name)?;
    let guard = db.read();
    let Stmt::Select(select) = stmt else {
        return Ok((None, None));
    };
    let plan = crate::exec::plan::plan_select(&guard, select)?;
    let physical = operator::build_statement(&guard, stmt)?
        .map(|op| operator::physical_tree(op.as_ref()));
    Ok((Some(plan), physical))
}

/// The `error` object shared by `/api/parse` and `/api/plan`.
fn error_json(error: Option<&(&'static str, Error)>) -> String {
    match error {
        None => "null".to_string(),
        Some((stage, error)) => {
            let (message, pos) = syntax_parts(error);
            format!(
                "{{\"stage\":{},\"message\":{},\"pos\":{}}}",
                json_string(stage),
                json_string(&message),
                // Null rather than 0: byte 0 is a real position, and a client
                // must be able to tell "no position" from "the very first byte".
                pos.map_or_else(|| "null".to_string(), |at| at.to_string()),
            )
        }
    }
}

/// Lexes then parses `sql`, collecting the token stream and the debug form of
/// every statement. The error, when present, names the stage that failed.
fn compile(sql: &str) -> (Vec<Token>, Vec<String>, Option<(&'static str, Error)>) {
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
    (tokens, statements, error)
}

/// Reads the `sql` string out of a JSON request body.
fn request_sql(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let request = json::parse(&text).ok()?;
    request.get("sql").and_then(Json::as_str).map(str::to_owned)
}

fn bad_request() -> Response {
    Response::json("400 Bad Request", encode_error("expected a string field \"sql\""))
}

fn string_or_null(value: Option<&str>) -> String {
    value.map_or_else(|| "null".to_string(), json_string)
}

// --------------------------------------------------------------- /api/config

fn config_trace(config: &Config) -> Response {
    Response::json(
        "200 OK",
        format!(
            "{{\"title\":{},\"page_preview\":{},\"admin_api\":{}}}",
            json_string(&config.web.title),
            config.web.page_preview,
            config.server.admin_api,
        ),
    )
}

// --------------------------------------------------------------- /api/schema

fn schema_trace(instance: &Instance) -> Response {
    let names = instance.databases().unwrap_or_default();
    let mut databases = Vec::new();
    for name in names {
        if name == crate::instance::INFORMATION_SCHEMA {
            continue;
        }
        let Ok(db) = instance.database(&name) else {
            continue;
        };
        let guard = db.read();
        let tables: Vec<String> = guard.catalog().table_metas().iter().map(table_json).collect();
        databases.push(format!(
            "{{\"name\":{},\"tables\":[{}]}}",
            json_string(&name),
            tables.join(",")
        ));
    }
    Response::json("200 OK", format!("{{\"databases\":[{}]}}", databases.join(",")))
}

fn table_json(meta: &TableMeta) -> String {
    let engine = match meta.engine {
        EngineKind::Heap => "heap",
        EngineKind::Lsm => "lsm",
    };
    let layout = match meta.layout {
        PageLayout::Row => "row",
        PageLayout::Pax => "pax",
    };
    let columns: Vec<String> = meta
        .columns
        .iter()
        .map(|c| {
            let default = match &c.default {
                Some(v) => json_string(&v.to_string()),
                None => "null".to_string(),
            };
            format!(
                "{{\"name\":{},\"type\":{},\"not_null\":{},\"primary_key\":{},\"unique\":{},\"default\":{}}}",
                json_string(&c.name),
                json_string(&c.dtype.to_string()),
                c.not_null,
                c.primary_key,
                c.unique,
                default,
            )
        })
        .collect();
    format!(
        "{{\"name\":{},\"engine\":{},\"layout\":{},\"columns\":[{}]}}",
        json_string(&meta.name),
        json_string(engine),
        json_string(layout),
        columns.join(",")
    )
}

// ---------------------------------------------------------------- /api/files

/// `GET /api/files?db=<name>` -- the files making up one database directory.
/// Gated by `web.page_preview`, which is off by default.
fn files_trace(instance: &Instance, config: &Config, query: &str) -> Response {
    if !config.web.page_preview {
        return Response::not_found();
    }
    let params = inspect::query_params(query);
    let Some(db) = inspect::param(&params, "db") else {
        return Response::not_found();
    };
    if !inspect::valid_db_name(db) {
        return Response::not_found();
    }
    let dir = instance.root().join(db);
    if !dir.is_dir() {
        return Response::not_found();
    }

    // `(name, json)` pairs so the output can be sorted by the forward-slash name.
    let mut files: Vec<(String, String)> = Vec::new();
    for (name, kind) in
        [("catalog.bin", "catalog"), ("wal.bin", "wal"), ("dwb.bin", "dwb")]
    {
        if let Some(json) = file_json(&dir.join(name), name, kind) {
            files.push((name.to_string(), json));
        }
    }
    for (fname, path) in subdir_entries(&dir, "tables") {
        let name = format!("tables/{fname}");
        if fname.ends_with(".dbf") {
            if let Some(json) = file_json(&path, &name, "heap") {
                files.push((name, json));
            }
        } else if fname.ends_with(".lsm") {
            // `.lsm` is a directory; its size is the sum of the SSTables in it.
            let size = dir_size(&path);
            files.push((name.clone(), entry_json(&name, "lsm", size, 0)));
        }
    }
    for (fname, path) in subdir_entries(&dir, "indexes") {
        if fname.ends_with(".idxf") {
            let name = format!("indexes/{fname}");
            if let Some(json) = file_json(&path, &name, "index") {
                files.push((name, json));
            }
        }
    }
    for (fname, path) in subdir_entries(&dir, "lobs") {
        if fname.ends_with(".lob") {
            let name = format!("lobs/{fname}");
            if let Some(json) = file_json(&path, &name, "lob") {
                files.push((name, json));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let body = format!(
        "{{\"db\":{},\"files\":[{}]}}",
        json_string(db),
        files.into_iter().map(|(_, json)| json).collect::<Vec<_>>().join(",")
    );
    Response::json("200 OK", body)
}

/// `(name, path)` for every entry directly inside `<dir>/<subdir>`.
fn subdir_entries(dir: &Path, subdir: &str) -> Vec<(String, std::path::PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir.join(subdir)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| Some((e.file_name().to_str()?.to_owned(), e.path())))
        .collect()
}

fn file_json(path: &Path, name: &str, kind: &str) -> Option<String> {
    let size = std::fs::metadata(path).ok()?.len();
    Some(entry_json(name, kind, size, page_count(name, size)))
}

fn entry_json(name: &str, kind: &str, size: u64, pages: u32) -> String {
    format!(
        "{{\"name\":{},\"kind\":{},\"size\":{},\"pages\":{}}}",
        json_string(name),
        json_string(kind),
        size,
        pages
    )
}

/// Whole pages in a file, for the fixed-page formats.
fn page_count(name: &str, size: u64) -> u32 {
    if name.ends_with(".dbf")
        || name.ends_with(".idxf")
        || name == "dwb.bin"
        || name.ends_with(".dwb")
    {
        size.div_ceil(crate::storage::page::PAGE_SIZE as u64) as u32
    } else {
        0
    }
}

fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let meta = entry.metadata();
        match meta {
            Ok(m) if m.is_dir() => total += dir_size(&entry.path()),
            Ok(m) => total += m.len(),
            Err(_) => {}
        }
    }
    total
}

// ------------------------------------------------------------------ json bits

fn join(items: impl Iterator<Item = String>) -> String {
    items.collect::<Vec<_>>().join(",")
}

fn tokens_json(tokens: &[Token]) -> String {
    join(tokens.iter().map(token_json))
}

fn statements_json(statements: &[String]) -> String {
    join(statements.iter().map(|debug| format!("{{\"debug\":{}}}", json_string(debug))))
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

/// The message and byte offset of a compiler complaint.
fn syntax_parts(error: &Error) -> (String, Option<usize>) {
    match error {
        Error::Syntax { message, pos } => (format!("syntax error: {message}"), *pos),
        other => (other.to_string(), None),
    }
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

// -------------------------------------------------------------- embedded demo

/// The built-in demo console, embedded in the binary. It takes precedence over
/// `web_root` static hosting whenever `web.enabled` is set.
fn embedded_asset(path: &str) -> Option<Response> {
    let (body, content_type) = match path {
        "/" | "/index.html" => (
            include_str!("../../web/demo/index.html"),
            "text/html; charset=utf-8",
        ),
        "/app.js" => (
            include_str!("../../web/demo/app.js"),
            "application/javascript; charset=utf-8",
        ),
        "/style.css" => (
            include_str!("../../web/demo/style.css"),
            "text/css; charset=utf-8",
        ),
        _ => return None,
    };
    let mut response = Response::new("200 OK", content_type, body.as_bytes().to_vec());
    response.extra_headers.push(("Cache-Control", "no-cache".to_string()));
    Some(response)
}

// -------------------------------------------------------------- static hosting

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
