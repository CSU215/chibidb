//! Static hosting for the built SPA, plus the `/api/*` surface.
//!
//! `http.rs` owns HTTP framing; this module owns the routes and their content.
//! [`handle`] returns `None` for the paths the legacy frontend already serves
//! (`/health` and `/query`), so those keep working untouched.

use std::path::Path;

use crate::config::Config;
use crate::result::encode_error;

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
pub(crate) fn handle(config: &Config, method: &str, path: &str, _body: &[u8]) -> Option<Response> {
    match (method, path) {
        // The legacy frontend keeps these two.
        ("GET", "/health") | ("POST", "/query") => None,
        // Reserved namespace: 404 unless the admin API is switched on.
        (_, path) if path.starts_with("/api/") => Some(Response::not_found()),
        ("GET", path) => Some(static_file(config, path)),
        _ => None,
    }
}

/// Serves one file from the configured web root.
fn static_file(config: &Config, path: &str) -> Response {
    let Some(root) = config.server.web_root.as_deref().filter(|root| !root.is_empty()) else {
        return Response::not_found();
    };
    // No percent-decoding: anything that would need it is rejected instead. Vite
    // emits ASCII with hashed names, so nothing legitimate is lost, and `%2e%2e`
    // cannot be smuggled through.
    if !path.is_ascii() || path.contains('%') {
        return Response::not_found();
    }
    let relative = path.trim_start_matches('/');
    let relative = if relative.is_empty() { "index.html" } else { relative };
    // Reject traversal before touching the filesystem.
    if relative.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..") {
        return Response::not_found();
    }
    let root = Path::new(root);
    let Ok(base) = root.canonicalize() else {
        return Response::not_found();
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
