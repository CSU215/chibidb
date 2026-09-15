//! Static hosting for the built SPA, plus the `/api/*` surface.
//!
//! `http.rs` owns HTTP framing; this module owns the routes and their content.
//! [`handle`] returns `None` for the paths the legacy frontend already serves
//! (`/health` and `/query`), so those keep working untouched.

use std::path::Path;

use crate::catalog::meta::{TableMeta, ViewMeta};
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

/// The `/api/*` surface. Off unless `server.admin_api` (for an external SPA) or
/// `web.enabled` (the built-in console needs it) is set, so the whole namespace
/// answers 404 on a default deployment. Raw page previews are still gated
/// separately by `web.page_preview`.
fn api(
    instance: &Instance,
    session: &Session,
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
) -> Response {
    let config = instance.config();
    if !(config.server.admin_api || config.web.enabled) {
        return Response::not_found();
    }
    match (method, path) {
        ("POST", "/api/parse") => parse_trace(body),
        ("POST", "/api/plan") => plan_trace(instance, session, body),
        ("GET", "/api/config") => config_trace(config),
        ("GET", "/api/schema") => schema_trace(instance),
        ("GET", "/api/buffer") => buffer_trace(instance, config),
        ("GET", "/api/files") => files_trace(instance, config, query),
        ("GET", "/api/overview") => inspect::overview(instance, config, query),
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
    // A fresh instance has no default database until the first statement
    // creates it, so there is nothing to plan against yet: report the token
    // stream and AST only rather than a "no such database" error.
    let Ok(db) = instance.database(db_name) else {
        return Ok((None, None));
    };
    let guard = db.read();
    let Stmt::Select(select) = stmt else {
        return Ok((None, None));
    };
    let plan = match crate::exec::logical::translate(select) {
        Some(node) => crate::exec::logical::optimize(&guard, node)?
            .map(|node| crate::exec::logical::logical_tree(&node)),
        None => None,
    };
    let physical = crate::exec::planner::plan_statement(&guard, stmt)?
        .map(|op| operator::physical_tree(op.as_ref()));
    Ok((plan, physical))
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
    let mut databases = Vec::new();
    for (name, system) in instance.database_names().unwrap_or_default() {
        if name == crate::instance::INFORMATION_SCHEMA {
            continue;
        }
        let Some(db) = inspect::db_handle(instance, &name) else {
            continue;
        };
        let guard = db.read();
        let tables: Vec<String> = guard.catalog().table_metas().iter().map(table_json).collect();
        let views: Vec<String> = guard.catalog().view_metas().iter().map(view_json).collect();
        databases.push(format!(
            "{{\"name\":{},\"system\":{},\"tables\":[{}],\"views\":[{}]}}",
            json_string(&name),
            system,
            tables.join(","),
            views.join(",")
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

fn view_json(meta: &ViewMeta) -> String {
    format!(
        "{{\"name\":{},\"sql\":{}}}",
        json_string(&meta.name),
        json_string(&meta.sql)
    )
}

// --------------------------------------------------------------- /api/buffer

/// `GET /api/buffer` -- a buffer-pool snapshot per database.
///
/// Every database owns its own pool, so the response lists one entry per open
/// database plus an aggregate. Counters are cumulative since the pool opened.
/// Safe to expose (no page contents), so it shares the `/api/*` gate with
/// `/api/schema` rather than the `web.page_preview` switch.
fn buffer_trace(instance: &Instance, config: &Config) -> Response {
    let mut pools = Vec::new();
    let mut total = crate::storage::PoolStats::default();
    for (name, system) in instance.database_names().unwrap_or_default() {
        if name == crate::instance::INFORMATION_SCHEMA {
            continue;
        }
        let Some(db) = inspect::db_handle(instance, &name) else {
            continue;
        };
        let stats = db.read().buffer_pool_stats();
        total.hits += stats.hits;
        total.misses += stats.misses;
        total.evictions += stats.evictions;
        total.dirty_evictions += stats.dirty_evictions;
        total.resident += stats.resident;
        total.capacity += stats.capacity;
        pools.push(format!(
            "{{\"name\":{},\"system\":{},{}}}",
            json_string(&name),
            system,
            pool_json(&stats)
        ));
    }
    Response::json(
        "200 OK",
        format!(
            "{{\"frame_size\":{},\"eviction\":{},\"databases\":[{}],\"total\":{{{}}}}}",
            crate::storage::PAGE_SIZE,
            json_string(crate::storage::buffer::policy_label(config.storage.eviction)),
            pools.join(","),
            pool_json(&total),
        ),
    )
}

/// The counters shared by one pool and the aggregate.
fn pool_json(stats: &crate::storage::PoolStats) -> String {
    format!(
        "\"hits\":{},\"misses\":{},\"evictions\":{},\"dirty_evictions\":{},\
         \"clean_evictions\":{},\"resident\":{},\"capacity\":{},\"hit_rate\":{:.4}",
        stats.hits,
        stats.misses,
        stats.evictions,
        stats.dirty_evictions,
        stats.clean_evictions(),
        stats.resident,
        stats.capacity,
        stats.hit_rate(),
    )
}

// ---------------------------------------------------------------- /api/files

/// `GET /api/files?db=<name>` -- the files making up one database directory.
/// Gated by `web.page_preview`, which is off by default. The system database
/// `chibi_meta` is listed like any other, resolved straight under the root.
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
    let system = db == crate::instance::META_DIR;

    // `(name, json)` pairs so the output can be sorted by the forward-slash name.
    let mut files: Vec<(String, String)> = catalog_entries(&dir);

    for (fname, path) in subdir_entries(&dir, "tables") {
        let name = format!("tables/{fname}");
        if fname.ends_with(".dbf") {
            files.push((name.clone(), heap_entry(instance, db, &fname, &name, &path)));
        } else if fname.ends_with(".lsm") {
            // An `.lsm` directory is listed by its contents, one level down.
            let table = inspect::table_name_for_file(instance, db, &fname);
            for (inner, ipath) in read_entries(&path) {
                let rel = format!("tables/{fname}/{inner}");
                let size = std::fs::metadata(&ipath).map(|m| m.len()).unwrap_or(0);
                let entry = if inner == "MANIFEST" {
                    Entry {
                        name: &rel,
                        kind: "lsm_manifest",
                        size,
                        unit_kind: "none",
                        units: 1,
                        engine: Some("lsm"),
                        layout: Some("row"),
                        table: table.as_deref(),
                        index: None,
                    }
                } else if inner.starts_with("sst-") && inner.ends_with(".sst") {
                    Entry {
                        name: &rel,
                        kind: "lsm_sstable",
                        size,
                        unit_kind: "region",
                        units: inspect::sstable_region_count(&ipath),
                        engine: Some("lsm"),
                        layout: Some("row"),
                        table: table.as_deref(),
                        index: None,
                    }
                } else {
                    continue;
                };
                let json = entry.json();
                files.push((rel, json));
            }
        }
    }
    for (fname, path) in subdir_entries(&dir, "indexes") {
        if fname.ends_with(".idxf") {
            let name = format!("indexes/{fname}");
            files.push((name.clone(), index_entry(instance, db, &fname, &name, &path)));
        }
    }
    for (fname, path) in subdir_entries(&dir, "lobs") {
        if fname.ends_with(".lob") {
            let name = format!("lobs/{fname}");
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let entry = Entry {
                name: &name,
                kind: "lob",
                size,
                unit_kind: "none",
                units: 1,
                engine: None,
                layout: None,
                table: None,
                index: None,
            };
            let json = entry.json();
            files.push((name, json));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let body = format!(
        "{{\"db\":{},\"system\":{},\"files\":[{}]}}",
        json_string(db),
        system,
        files.into_iter().map(|(_, json)| json).collect::<Vec<_>>().join(",")
    );
    Response::json("200 OK", body)
}

/// One file in an `/api/files` listing.
struct Entry<'a> {
    name: &'a str,
    kind: &'a str,
    size: u64,
    unit_kind: &'a str,
    units: u32,
    engine: Option<&'a str>,
    layout: Option<&'a str>,
    table: Option<&'a str>,
    index: Option<&'a str>,
}

impl Entry<'_> {
    fn json(&self) -> String {
        format!(
            "{{\"name\":{},\"kind\":{},\"size\":{},\"unit_kind\":{},\"units\":{},\
             \"engine\":{},\"layout\":{},\"table\":{},\"index\":{}}}",
            json_string(self.name),
            json_string(self.kind),
            self.size,
            json_string(self.unit_kind),
            self.units,
            opt_str(self.engine),
            opt_str(self.layout),
            opt_str(self.table),
            opt_str(self.index),
        )
    }
}

fn opt_str(value: Option<&str>) -> String {
    value.map_or_else(|| "null".to_string(), json_string)
}

/// The catalog/wal/dwb files at the root of a database directory.
fn catalog_entries(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, kind) in
        [("catalog.bin", "catalog"), ("wal.bin", "wal"), ("dwb.bin", "dwb")]
    {
        let path = dir.join(name);
        if !path.is_file() {
            continue;
        }
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let (unit_kind, units) = match kind {
            "wal" => ("frame", inspect::unit_count_for_path(kind, &path)),
            "dwb" => ("record", inspect::unit_count_for_path(kind, &path)),
            _ => ("page", 1),
        };
        let entry = Entry {
            name,
            kind,
            size,
            unit_kind,
            units,
            engine: None,
            layout: None,
            table: None,
            index: None,
        };
        out.push((name.to_string(), entry.json()));
    }
    out
}

fn heap_entry(instance: &Instance, db: &str, fname: &str, name: &str, path: &Path) -> String {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let units = size.div_ceil(crate::storage::PAGE_SIZE as u64) as u32;
    let meta = inspect::table_for_dbf(instance, db, fname);
    let entry = Entry {
        name,
        kind: "heap",
        size,
        unit_kind: "page",
        units,
        engine: Some(meta.as_ref().map_or("heap", |m| inspect::engine_str(m.engine))),
        layout: meta.as_ref().map(|m| inspect::layout_str(m.layout)),
        table: meta.as_ref().map(|m| m.name.as_str()),
        index: None,
    };
    entry.json()
}

fn index_entry(instance: &Instance, db: &str, fname: &str, name: &str, path: &Path) -> String {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let units = size.div_ceil(crate::storage::PAGE_SIZE as u64) as u32;
    let meta = inspect::index_for_idxf(instance, db, fname);
    let entry = Entry {
        name,
        kind: "index",
        size,
        unit_kind: "page",
        units,
        engine: None,
        layout: None,
        table: meta.as_ref().map(|m| m.table.as_str()),
        index: meta.as_ref().map(|m| m.name.as_str()),
    };
    entry.json()
}

/// `(name, path)` for every entry directly inside `<dir>/<subdir>`.
fn subdir_entries(dir: &Path, subdir: &str) -> Vec<(String, std::path::PathBuf)> {
    read_entries(&dir.join(subdir))
}

fn read_entries(dir: &Path) -> Vec<(String, std::path::PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| Some((e.file_name().to_str()?.to_owned(), e.path())))
        .collect()
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
        "<!doctype html>\n<meta charset=\"utf-8\">\n<title>chaoticdb</title>\n\
         <h1>chaoticdb</h1>\n\
         <p>No frontend is served: <code>server.web_root</code> points at\n\
         <code>{}</code>, which does not exist.</p>\n\
         <p>Set <code>[web] enabled = true</code> in <code>config.toml</code> to use the\n\
         built-in console (no build step), or put a built SPA in that directory.</p>\n\
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
