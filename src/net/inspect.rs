//! Read-only disk inspection for the demo console: `/api/files` and
//! `/api/page`.
//!
//! This module only ever reads files, but it reads *raw* files, so it is gated
//! behind `web.page_preview` (off by default). Every path is validated and
//! canonicalized before a byte is read, so a request cannot escape the database
//! directory.

use std::path::{Component, Path};

use crate::config::{Config, PageLayout};
use crate::instance::Instance;
use crate::storage::PAGE_SIZE;

use super::admin::Response;
use super::json::json_string;

/// Shown alongside every page dump, so a confusing result is explained in
/// place.
const NOTES: &str = "raw file read; unflushed buffer-pool pages are not shown";

// ------------------------------------------------------------- query helpers

/// Parses `k=v&k=v`, percent-decoding both sides. A pair without `=` has an
/// empty value.
pub(crate) fn query_params(query: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.push((percent_decode(key), percent_decode(value)));
    }
    out
}

/// The first value for `key`.
pub(crate) fn param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Database names are identifiers; rejecting anything else also blocks path
/// traversal through the `db` parameter.
pub(crate) fn valid_db_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// -------------------------------------------------------------------- /api/page

/// `GET /api/page?db=<name>&file=<relpath>&no=<n>` -- one 8192-byte page, hex
/// and ASCII dumps included, plus a best-effort structural decode.
pub(crate) fn page(instance: &Instance, config: &Config, query: &str) -> Response {
    if !config.web.page_preview {
        return Response::not_found();
    }
    let params = query_params(query);
    let (Some(db), Some(file), Some(no)) = (
        param(&params, "db"),
        param(&params, "file"),
        param(&params, "no"),
    ) else {
        return Response::not_found();
    };
    if !valid_db_name(db) || !valid_relative_path(file) {
        return Response::not_found();
    }
    let Ok(no) = no.parse::<u32>() else {
        return Response::not_found();
    };
    let Ok(base) = instance.root().join(db).canonicalize() else {
        return Response::not_found();
    };
    if !base.is_dir() {
        return Response::not_found();
    }
    let Ok(resolved) = base.join(file).canonicalize() else {
        return Response::not_found();
    };
    if !resolved.starts_with(&base) || !resolved.is_file() {
        return Response::not_found();
    }

    let file_size = std::fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
    let total_pages = file_size.div_ceil(PAGE_SIZE as u64) as u32;
    let bytes = read_page(&resolved, no);
    let kind = kind_for(file);
    let header = header_object(&bytes);
    let structure = structure(instance, db, file, no, &bytes, &resolved);

    let mut out = String::from("{\"db\":");
    out.push_str(&json_string(db));
    out.push_str(",\"file\":");
    out.push_str(&json_string(file));
    out.push_str(&format!(",\"no\":{no}"));
    out.push_str(&format!(",\"page_size\":{PAGE_SIZE}"));
    out.push_str(&format!(",\"total_pages\":{total_pages}"));
    out.push_str(",\"kind\":");
    out.push_str(&json_string(kind));
    out.push_str(",\"header\":");
    out.push_str(&match &header {
        Some(inner) => format!("{{{inner}}}"),
        None => "null".to_string(),
    });
    out.push_str(",\"structure\":");
    out.push_str(&structure);
    out.push_str(",\"hex\":");
    out.push_str(&json_string(&hex_dump(&bytes)));
    out.push_str(",\"ascii\":");
    out.push_str(&json_string(&ascii_dump(&bytes)));
    out.push_str(",\"notes\":");
    out.push_str(&json_string(NOTES));
    out.push('}');
    Response::json("200 OK", out)
}

/// A relative path with no parent, root or prefix components.
fn valid_relative_path(file: &str) -> bool {
    if file.is_empty() || file.contains('\0') || file.contains(':') {
        return false;
    }
    Path::new(file)
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
}

/// Reads up to one page at `no`, returning fewer bytes at end of file.
fn read_page(path: &Path, no: u32) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    if file.seek(SeekFrom::Start(no as u64 * PAGE_SIZE as u64)).is_err() {
        return Vec::new();
    }
    let mut buf = vec![0u8; PAGE_SIZE];
    let mut filled = 0;
    while filled < PAGE_SIZE {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => break,
        }
    }
    buf.truncate(filled);
    buf
}

fn kind_for(file: &str) -> &'static str {
    if file == "catalog.bin" {
        "catalog"
    } else if file == "wal.bin" {
        "wal"
    } else if file == "dwb.bin" {
        "dwb"
    } else if file.ends_with(".dbf") {
        "heap"
    } else if file.ends_with(".lsm") {
        "lsm"
    } else if file.ends_with(".idxf") {
        "index"
    } else if file.ends_with(".lob") {
        "lob"
    } else {
        "other"
    }
}

// ------------------------------------------------------------------ hex / ascii

fn hex_dump(bytes: &[u8]) -> String {
    bytes
        .chunks(16)
        .map(|chunk| {
            chunk.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn ascii_dump(bytes: &[u8]) -> String {
    bytes
        .chunks(16)
        .map(|chunk| {
            chunk
                .iter()
                .map(|&b| if (0x20..=0x7e).contains(&b) { b as char } else { '.' })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ------------------------------------------------------------------ structure

/// The common 15-byte file header, if `bytes` starts with one. The check is
/// exact so a data page cannot be mistaken for a header.
fn header_object(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 15 {
        return None;
    }
    if u32::from_le_bytes(bytes[11..15].try_into().unwrap()) != PAGE_SIZE as u32 {
        return None;
    }
    let kind = match bytes[10] {
        0 => "heap",
        1 => "index",
        2 => "catalog",
        _ => return None,
    };
    if !bytes[0..8].iter().all(|&b| (0x20..=0x7e).contains(&b)) {
        return None;
    }
    let magic = String::from_utf8_lossy(&bytes[0..8]).into_owned();
    let version = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
    Some(format!(
        "\"magic\":{},\"format_version\":{},\"file_kind\":{},\"page_size\":{}",
        json_string(&magic),
        version,
        json_string(kind),
        PAGE_SIZE
    ))
}

fn unknown() -> String {
    "{\"type\":\"unknown\"}".to_string()
}

fn structure(
    instance: &Instance,
    db: &str,
    file: &str,
    no: u32,
    page: &[u8],
    resolved: &Path,
) -> String {
    // The WAL is a sequence of frames from byte 0, so it is parsed whole rather
    // than from the requested page.
    if file == "wal.bin" {
        let bytes = std::fs::read(resolved).unwrap_or_default();
        return wal_structure(&bytes);
    }
    let has_file_header = file == "catalog.bin" || file.ends_with(".dbf") || file.ends_with(".idxf");
    if no == 0 && has_file_header {
        return match header_object(page) {
            Some(inner) => format!("{{\"type\":\"file_header\",{inner}}}"),
            None => unknown(),
        };
    }
    if file.ends_with(".dbf") && no >= 1 {
        return match table_layout(instance, db, file) {
            Some(PageLayout::Pax) => pax_structure(page),
            Some(PageLayout::Row) => slotted_structure(page),
            None => unknown(),
        };
    }
    if file.ends_with(".idxf") && no >= 1 {
        return btree_structure(page);
    }
    unknown()
}

/// `tables/NNNNNN.dbf` -> the layout recorded for file `NNNNNN`.
fn table_layout(instance: &Instance, db: &str, file: &str) -> Option<PageLayout> {
    let id: u32 = file.rsplit('/').next()?.strip_suffix(".dbf")?.parse().ok()?;
    let handle = instance.database(db).ok()?;
    let guard = handle.read();
    guard
        .catalog()
        .table_metas()
        .into_iter()
        .find(|meta| meta.file == id)
        .map(|meta| meta.layout)
}

fn pax_structure(page: &[u8]) -> String {
    format!("{{\"type\":\"pax\",\"num_slots\":{}}}", u16_at(page, 0))
}

fn slotted_structure(page: &[u8]) -> String {
    if page.len() < 4 {
        return unknown();
    }
    let num_slots = u16_at(page, 0) as usize;
    let raw_free_upper = u16_at(page, 2) as usize;
    let free_upper = if raw_free_upper == 0 { PAGE_SIZE } else { raw_free_upper };
    let dir_end = 4 + num_slots * 4;
    if dir_end > PAGE_SIZE || dir_end > free_upper {
        return unknown();
    }
    let mut slots = Vec::with_capacity(num_slots);
    for i in 0..num_slots {
        let base = 4 + i * 4;
        let offset = u16_at(page, base);
        let length = u16_at(page, base + 2);
        let status = if offset == 0 && length == 0 { "free" } else { "live" };
        slots.push(format!(
            "{{\"slot\":{i},\"offset\":{offset},\"length\":{length},\"status\":{}}}",
            json_string(status)
        ));
    }
    format!(
        "{{\"type\":\"slotted\",\"num_slots\":{num_slots},\"free_upper\":{free_upper},\
         \"slot_end\":{free_upper},\"free_bytes\":{},\"slots\":[{}]}}",
        free_upper.saturating_sub(dir_end),
        slots.join(",")
    )
}

fn btree_structure(page: &[u8]) -> String {
    let node_type = if page.first() == Some(&0) { "leaf" } else { "internal" };
    format!(
        "{{\"type\":\"btree_node\",\"node_type\":{},\"entries\":{},\"prev\":{},\
         \"next\":{},\"high_key_len\":{}}}",
        json_string(node_type),
        u16_at(page, 1),
        u32_at(page, 3),
        u32_at(page, 7),
        u16_at(page, 11)
    )
}

/// Counts well-formed frames from offset 0, stopping at the first truncated or
/// unknown one, exactly as recovery does.
fn wal_structure(bytes: &[u8]) -> String {
    let mut pos = 0usize;
    let mut frames = 0u32;
    while pos + 4 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        // `len` counts the type byte, the trx id and the payload.
        if len < 9 || pos + 4 + len > bytes.len() {
            break;
        }
        if !matches!(bytes[pos + 4], 1..=3) {
            break;
        }
        frames += 1;
        pos += 4 + len;
    }
    format!("{{\"type\":\"wal\",\"frames\":{frames}}}")
}

fn u16_at(page: &[u8], off: usize) -> u16 {
    if off + 2 > page.len() {
        return 0;
    }
    u16::from_le_bytes([page[off], page[off + 1]])
}

fn u32_at(page: &[u8], off: usize) -> u32 {
    if off + 4 > page.len() {
        return 0;
    }
    u32::from_le_bytes(page[off..off + 4].try_into().unwrap())
}
