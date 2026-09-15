//! Read-only disk inspection for the demo console: `/api/files`,
//! `/api/overview` and `/api/page`.
//!
//! This module only ever reads files, but it reads *raw* files, so it is gated
//! behind `web.page_preview` (off by default). Every path is validated and
//! canonicalized before a byte is read, so a request cannot escape the database
//! directory.

use std::path::{Component, Path};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::catalog::meta::{IndexMeta, TableMeta};
use crate::config::{Config, EngineKind, PageLayout};
use crate::instance::Instance;
use crate::storage::PAGE_SIZE;
use crate::storage::lsm::block::Block;
use crate::storage::lsm::coding::get_varint64;
use crate::Database;

use super::admin::Response;
use super::json::json_string;

/// Shown alongside every page dump, so a confusing result is explained in
/// place.
const NOTES: &str = "raw file read; unflushed buffer-pool pages are not shown";

/// A high bit tags index file ids so they never collide with table files.
const INDEX_FILE_TAG: u32 = 1 << 31;

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

// -------------------------------------------------------------- catalog lookup

/// The handle for `db`, resolving the system database to its instance handle.
pub(crate) fn db_handle(instance: &Instance, db: &str) -> Option<Arc<RwLock<Database>>> {
    if db == crate::instance::META_DIR {
        Some(instance.meta())
    } else {
        instance.database(db).ok()
    }
}

fn table_metas(instance: &Instance, db: &str) -> Vec<TableMeta> {
    match db_handle(instance, db) {
        Some(handle) => handle.read().catalog().table_metas(),
        None => Vec::new(),
    }
}

fn index_metas(instance: &Instance, db: &str) -> Vec<IndexMeta> {
    match db_handle(instance, db) {
        Some(handle) => handle.read().catalog().index_metas(),
        None => Vec::new(),
    }
}

/// Table metadata for the heap file `tables/NNNNNN.dbf`.
pub(crate) fn table_for_dbf(instance: &Instance, db: &str, fname: &str) -> Option<TableMeta> {
    let no = basename(fname).strip_suffix(".dbf")?.parse::<u32>().ok()?;
    table_metas(instance, db).into_iter().find(|m| m.file == no)
}

/// Index metadata for the index file `indexes/NNNNNN.idxf`.
pub(crate) fn index_for_idxf(instance: &Instance, db: &str, fname: &str) -> Option<IndexMeta> {
    let no = basename(fname).strip_suffix(".idxf")?.parse::<u32>().ok()?;
    index_metas(instance, db)
        .into_iter()
        .find(|m| m.file & !INDEX_FILE_TAG == no)
}

/// Table name owning the LSM directory `tables/NNNNNN.lsm`.
pub(crate) fn table_name_for_file(instance: &Instance, db: &str, fname: &str) -> Option<String> {
    let no = basename(fname).strip_suffix(".lsm")?.parse::<u32>().ok()?;
    table_metas(instance, db)
        .into_iter()
        .find(|m| m.file == no)
        .map(|m| m.name)
}

pub(crate) fn engine_str(engine: EngineKind) -> &'static str {
    match engine {
        EngineKind::Heap => "heap",
        EngineKind::Lsm => "lsm",
    }
}

pub(crate) fn layout_str(layout: PageLayout) -> &'static str {
    match layout {
        PageLayout::Row => "row",
        PageLayout::Pax => "pax",
    }
}

// -------------------------------------------------------------------- classes

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Catalog,
    Wal,
    Dwb,
    Heap,
    Index,
    LsmManifest,
    LsmSstable,
    Lob,
    Other,
}

fn classify(file: &str) -> Class {
    let name = basename(file);
    if name == "catalog.bin" {
        Class::Catalog
    } else if name == "wal.bin" {
        Class::Wal
    } else if name == "dwb.bin" {
        Class::Dwb
    } else if name == "MANIFEST" || name == "MANIFEST.tmp" {
        Class::LsmManifest
    } else if name.ends_with(".sst") {
        Class::LsmSstable
    } else if name.ends_with(".dbf") {
        Class::Heap
    } else if name.ends_with(".idxf") {
        Class::Index
    } else if name.ends_with(".lob") {
        Class::Lob
    } else {
        Class::Other
    }
}

fn kind_str(class: Class) -> &'static str {
    match class {
        Class::Catalog => "catalog",
        Class::Wal => "wal",
        Class::Dwb => "dwb",
        Class::Heap => "heap",
        Class::Index => "index",
        Class::LsmManifest => "lsm_manifest",
        Class::LsmSstable => "lsm_sstable",
        Class::Lob => "lob",
        Class::Other => "other",
    }
}

fn unit_kind_str(class: Class) -> &'static str {
    match class {
        Class::Catalog | Class::Heap | Class::Index => "page",
        Class::Wal => "frame",
        Class::Dwb => "record",
        Class::LsmSstable => "region",
        Class::LsmManifest | Class::Lob | Class::Other => "none",
    }
}

/// The visualization mode for the file's contents, independent of the unit.
fn base_mode(instance: &Instance, db: &str, file: &str, class: Class) -> &'static str {
    match class {
        Class::Catalog => "catalog",
        Class::Wal => "wal",
        Class::Dwb => "dwb",
        Class::LsmManifest => "lsm-manifest",
        Class::LsmSstable => "lsm-sstable",
        Class::Heap => match table_for_dbf(instance, db, file).map(|m| m.layout) {
            Some(PageLayout::Pax) => "heap-pax",
            _ => "heap-row",
        },
        Class::Index => "btree",
        Class::Lob | Class::Other => "unknown",
    }
}

fn description_for(mode: &str) -> &'static str {
    match mode {
        "heap-row" => {
            "行存堆表：页首是槽目录（每槽 4 字节，记录记录的偏移与长度），记录从页尾向前生长，\
             页头保存槽数与空闲上界。适合整行读写。"
        }
        "heap-pax" => {
            "PAX 列存堆表：页头之后是段目录，段 0 保存每行的版本三元组（creator/deleter/next_rid），\
             其余段按列存放，同一列的数据连续，适合只读取部分列的扫描。"
        }
        "btree" => {
            "B+ 树索引节点：页头包含节点类型、条目数与左右兄弟页号，高键用于定界；条目按 \
             (键, rid) 升序排列，叶子存 rid，内部节点存分隔键与子页号。"
        }
        "catalog" => {
            "数据库目录：以公共文件头开始，随后依次是下一个表文件号、下一个索引文件号、\
             下一个事务号、clog 基线，以及已提交事务、表、索引、视图等变长节。"
        }
        "wal" => {
            "预写日志：由若干帧组成，每帧为 [长度 u32][类型 u8][事务号 u64][负载]；\
             类型包括插入、删除标记与提交，只有带提交帧的事务会被重放。"
        }
        "dwb" => {
            "双写缓冲：每条记录为 [路径长度 u32][路径][页号 u32][8192 字节页镜像]，\
             用于崩溃后修复未写完的页。"
        }
        "lsm-sstable" => {
            "LSM 有序表：数据块在前，随后是布隆过滤器、索引块与尾部；索引把每个数据块的\
             末键映射到 (offset, size)，尾部记录过滤器与索引块的位置及魔数。"
        }
        "lsm-manifest" => {
            "LSM 清单：记录下一张表号与各层存活的表号，采用临时文件写入后原子重命名，\
             崩溃时只会看到旧清单或新清单。"
        }
        "file-header" => "数据文件公共头：魔数(8)、格式版本(2)、文件类型(1) 与页大小(4)。",
        _ => "无法识别的结构。",
    }
}

// -------------------------------------------------------------------- /api/page

/// `GET /api/page?db=<name>&file=<relpath>&no=<unit>` -- one unit (a page, a
/// WAL frame, a DWB record, an SSTable region or the manifest), hex and ASCII
/// dumps included, plus a best-effort structural decode and byte spans.
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

    flush_db(instance, db);
    let class = classify(file);
    let (chunk, region) = read_unit(class, &resolved, no);
    let total = unit_count(class, &resolved);
    let header = header_object(&chunk);
    let mode = if header.is_some() && matches!(class, Class::Heap | Class::Index) {
        "file-header"
    } else {
        base_mode(instance, db, file, class)
    };
    // An SSTable unit is one region; decode that region's bytes on their own.
    let (structure, fields) = match region {
        Some(kind) => {
            let (structure, region_fields) = sstable_region_decode(kind, &chunk);
            (structure, fields_json(chunk.len(), &region_fields))
        }
        None => (
            structure_json(mode, &chunk),
            fields_json(chunk.len(), &mode_fields(mode, &chunk)),
        ),
    };

    let mut out = String::from("{\"db\":");
    out.push_str(&json_string(db));
    out.push_str(",\"file\":");
    out.push_str(&json_string(file));
    out.push_str(&format!(",\"no\":{no}"));
    out.push_str(&format!(",\"page_size\":{PAGE_SIZE}"));
    out.push_str(&format!(",\"total_pages\":{total}"));
    out.push_str(",\"kind\":");
    out.push_str(&json_string(kind_str(class)));
    out.push_str(",\"unit_kind\":");
    out.push_str(&json_string(unit_kind_str(class)));
    out.push_str(",\"mode\":");
    out.push_str(&json_string(mode));
    out.push_str(",\"description\":");
    out.push_str(&json_string(description_for(mode)));
    out.push_str(",\"header\":");
    out.push_str(&match &header {
        Some(inner) => format!("{{{inner}}}"),
        None => "null".to_string(),
    });
    out.push_str(",\"structure\":");
    out.push_str(&structure);
    out.push_str(",\"fields\":");
    out.push_str(&fields);
    out.push_str(",\"hex\":");
    out.push_str(&json_string(&hex_dump(&chunk)));
    out.push_str(",\"ascii\":");
    out.push_str(&json_string(&ascii_dump(&chunk)));
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

// ------------------------------------------------------------------ /api/overview

/// `GET /api/overview?db=<name>&file=<relpath>` -- a per-unit occupancy grid
/// for one file.
pub(crate) fn overview(instance: &Instance, config: &Config, query: &str) -> Response {
    if !config.web.page_preview {
        return Response::not_found();
    }
    let params = query_params(query);
    let (Some(db), Some(file)) = (param(&params, "db"), param(&params, "file")) else {
        return Response::not_found();
    };
    if !valid_db_name(db) || !valid_relative_path(file) {
        return Response::not_found();
    }
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

    flush_db(instance, db);
    let class = classify(file);
    let mode = base_mode(instance, db, file, class);
    let (total, units) = overview_units(class, &resolved, mode);
    let body = format!(
        "{{\"db\":{},\"file\":{},\"kind\":{},\"mode\":{},\"description\":{},\
         \"unit_kind\":{},\"total\":{},\"units\":[{}]}}",
        json_string(db),
        json_string(file),
        json_string(kind_str(class)),
        json_string(mode),
        json_string(description_for(mode)),
        json_string(unit_kind_str(class)),
        total,
        units.join(",")
    );
    Response::json("200 OK", body)
}

fn overview_units(class: Class, resolved: &Path, mode: &str) -> (u32, Vec<String>) {
    match class {
        Class::Heap | Class::Index => {
            let size = file_size(resolved);
            let total = size.div_ceil(PAGE_SIZE as u64) as u32;
            let mut units = Vec::with_capacity(total as usize);
            for i in 0..total {
                let page = read_page(resolved, i);
                let (kind, used, summary) = if i == 0 && header_object(&page).is_some() {
                    ("file_header", true, "文件头".to_string())
                } else {
                    page_classify(mode, &page)
                };
                units.push(unit_json(i, kind, used, &summary));
            }
            (total, units)
        }
        Class::Catalog => (1, vec![unit_json(0, "catalog", true, "目录")]),
        Class::Wal => {
            let bytes = read_all(resolved);
            let frames = wal_frames(&bytes);
            let mut units = Vec::with_capacity(frames.len());
            for (i, &(_pos, _len, ty, trx)) in frames.iter().enumerate() {
                let (kind, what) = match ty {
                    1 => ("insert", "插入"),
                    2 => ("delete", "删除标记"),
                    3 => ("commit", "提交"),
                    _ => ("unknown", "未知"),
                };
                units.push(unit_json(i as u32, kind, true, &format!("{what} trx={trx}")));
            }
            (frames.len() as u32, units)
        }
        Class::Dwb => {
            let bytes = read_all(resolved);
            let records = dwb_records(&bytes);
            let mut units = Vec::with_capacity(records.len());
            for (i, &(pos, _total)) in records.iter().enumerate() {
                let path_len = u32_at(&bytes, pos) as usize;
                let page_no = u32_at(&bytes, pos + 4 + path_len);
                units.push(unit_json(i as u32, "record", true, &format!("页 {page_no}")));
            }
            (records.len() as u32, units)
        }
        Class::LsmSstable => {
            let bytes = read_all(resolved);
            let regions = sstable_regions(&bytes);
            let mut units = Vec::with_capacity(regions.len());
            for (i, (kind, _start, len)) in regions.iter().enumerate() {
                units.push(unit_json(i as u32, kind, true, &region_summary(kind, i, *len)));
            }
            (regions.len() as u32, units)
        }
        Class::LsmManifest => (1, vec![unit_json(0, "manifest", true, "清单")]),
        Class::Lob | Class::Other => {
            let used = file_size(resolved) > 0;
            (1, vec![unit_json(0, "data", used, if used { "数据" } else { "未使用" })])
        }
    }
}

fn unit_json(index: u32, kind: &str, used: bool, summary: &str) -> String {
    format!(
        "{{\"index\":{index},\"label\":{},\"kind\":{},\"used\":{used},\"summary\":{}}}",
        json_string(&index.to_string()),
        json_string(kind),
        json_string(summary),
    )
}

/// Classify one data page of a heap or index file.
fn page_classify(mode: &str, page: &[u8]) -> (&'static str, bool, String) {
    if page.is_empty() || page.iter().all(|&b| b == 0) {
        return ("empty", false, "未使用".to_string());
    }
    match mode {
        "heap-row" => {
            let n = u16_at(page, 0) as usize;
            if n == 0 {
                return ("empty", false, "未使用".to_string());
            }
            let raw_upper = u16_at(page, 2) as usize;
            let upper = if raw_upper == 0 { PAGE_SIZE } else { raw_upper };
            let dir_end = 4 + n * 4;
            let free = upper.saturating_sub(dir_end);
            let mut live = 0;
            for i in 0..n {
                let base = 4 + i * 4;
                if u16_at(page, base) != 0 || u16_at(page, base + 2) != 0 {
                    live += 1;
                }
            }
            ("slotted", true, format!("{live} 行，剩余 {free} B"))
        }
        "heap-pax" => {
            if !crate::storage::pax::is_pax(page) {
                return ("empty", false, "未使用".to_string());
            }
            let slots = crate::storage::pax::slot_count(page);
            if slots == 0 {
                return ("empty", false, "未使用".to_string());
            }
            let live = crate::storage::pax::alive_slots(page).count();
            ("pax", true, format!("{live} 行"))
        }
        "btree" => {
            let leaf = page.first() == Some(&0);
            let n = u16_at(page, 1);
            let kind = if leaf { "leaf" } else { "internal" };
            (kind, true, format!("{} 节点，{n} 条", if leaf { "叶" } else { "内" }))
        }
        _ => ("data", true, "数据".to_string()),
    }
}

/// Passed to `/api/files` to size an sstable without exposing the parser.
pub(crate) fn sstable_region_count(path: &Path) -> u32 {
    sstable_regions(&read_all(path)).len() as u32
}

/// Unit count for `catalog.bin`/`wal.bin`/`dwb.bin`.
pub(crate) fn unit_count_for_path(kind: &str, path: &Path) -> u32 {
    match kind {
        "wal" => wal_frames(&read_all(path)).len() as u32,
        "dwb" => dwb_records(&read_all(path)).len() as u32,
        _ => 1,
    }
}

// ----------------------------------------------------------------------- units

/// Makes recent writes visible to the raw-disk inspector: the endpoints read
/// files directly, so pages still sitting in the buffer pool (or an LSM
/// memtable) are flushed first. Failures are ignored -- a read-only or
/// freshly-created database simply has nothing to flush.
fn flush_db(instance: &Instance, db: &str) {
    let handle = if db == crate::instance::META_DIR {
        instance.meta()
    } else {
        match instance.database(db) {
            Ok(handle) => handle,
            Err(_) => return,
        }
    };
    let _ = handle.read().flush_pages();
}

/// Reads unit `no`. The second element is the region kind for an SSTable unit
/// (`data`/`bloom`/`index`/`footer`), so the caller decodes that region alone
/// instead of the whole file.
fn read_unit(class: Class, resolved: &Path, no: u32) -> (Vec<u8>, Option<&'static str>) {
    match class {
        Class::Heap | Class::Index => (read_page(resolved, no), None),
        Class::Wal => {
            let bytes = read_all(resolved);
            let chunk = wal_frames(&bytes)
                .get(no as usize)
                .and_then(|&(pos, len, _, _)| bytes.get(pos..pos + 4 + len))
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            (chunk, None)
        }
        Class::Dwb => {
            let bytes = read_all(resolved);
            let chunk = dwb_records(&bytes)
                .get(no as usize)
                .and_then(|&(pos, total)| bytes.get(pos..pos + total))
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            (chunk, None)
        }
        Class::LsmSstable => {
            let bytes = read_all(resolved);
            match sstable_regions(&bytes).get(no as usize) {
                Some(&(kind, start, len)) => (
                    bytes.get(start..start + len).map(<[u8]>::to_vec).unwrap_or_default(),
                    Some(kind),
                ),
                None => (Vec::new(), None),
            }
        }
        Class::Catalog | Class::LsmManifest | Class::Lob | Class::Other => {
            (read_all(resolved), None)
        }
    }
}

fn unit_count(class: Class, resolved: &Path) -> u32 {
    match class {
        Class::Heap | Class::Index => file_size(resolved).div_ceil(PAGE_SIZE as u64) as u32,
        Class::Wal => wal_frames(&read_all(resolved)).len() as u32,
        Class::Dwb => dwb_records(&read_all(resolved)).len() as u32,
        Class::LsmSstable => sstable_regions(&read_all(resolved)).len() as u32,
        Class::Catalog | Class::LsmManifest | Class::Lob | Class::Other => 1,
    }
}

fn read_all(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
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

/// `(start, frame_len)` for each well-formed WAL frame, where `frame_len` is
/// the u32 length field (bytes after the length itself).
fn wal_frames(bytes: &[u8]) -> Vec<(usize, usize, u8, u64)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= bytes.len() {
        let len = u32_at(bytes, pos) as usize;
        if len < 9 || pos + 4 + len > bytes.len() {
            break;
        }
        let ty = match bytes.get(pos + 4) {
            Some(&b) if (1..=3).contains(&b) => b,
            _ => break,
        };
        out.push((pos, len, ty, u64_at(bytes, pos + 5)));
        pos += 4 + len;
    }
    out
}

/// `(start, total_len)` for each complete DWB record.
fn dwb_records(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= bytes.len() {
        let path_len = u32_at(bytes, pos) as usize;
        let total = 4 + path_len + 4 + PAGE_SIZE;
        if pos + total > bytes.len() {
            break;
        }
        out.push((pos, total));
        pos += total;
    }
    out
}

/// `(kind, start, len)` for each region of an SSTable: the data blocks from
/// the index, then the bloom filter, the index block and the footer.
fn sstable_regions(chunk: &[u8]) -> Vec<(&'static str, usize, usize)> {
    let mut out = Vec::new();
    if chunk.len() < 40 {
        return out;
    }
    let footer = chunk.len() - 40;
    let filter_off = u64_at(chunk, footer) as usize;
    let filter_size = u64_at(chunk, footer + 8) as usize;
    let index_off = u64_at(chunk, footer + 16) as usize;
    let index_size = u64_at(chunk, footer + 24) as usize;
    for (off, size) in index_handles(chunk, index_off, index_size) {
        out.push(("data", off, size));
    }
    out.push(("bloom", filter_off, filter_size));
    out.push(("index", index_off, index_size));
    out.push(("footer", footer, 40));
    out
}

/// `(offset, size)` of every data block, decoded from the index block.
fn index_handles(chunk: &[u8], offset: usize, size: usize) -> Vec<(usize, usize)> {
    let Some(end) = offset.checked_add(size) else {
        return Vec::new();
    };
    let Some(bytes) = chunk.get(offset..end) else {
        return Vec::new();
    };
    let Ok(block) = Block::parse(bytes.to_vec()) else {
        return Vec::new();
    };
    let Ok(entries) = block.entries() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (_key, encoded) in entries {
        let mut pos = 0;
        let (Ok(off), Ok(len)) = (
            get_varint64(&encoded, &mut pos),
            get_varint64(&encoded, &mut pos),
        ) else {
            continue;
        };
        out.push((off as usize, len as usize));
    }
    out
}

fn region_summary(kind: &str, index: usize, len: usize) -> String {
    match kind {
        "data" => format!("数据块 {index} ({len} B)"),
        "bloom" => format!("布隆过滤器 ({len} B)"),
        "index" => format!("索引块 ({len} B)"),
        "footer" => "尾部 (40 B)".to_string(),
        _ => format!("区域 ({len} B)"),
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
    if u32_at(bytes, 11) != PAGE_SIZE as u32 {
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
    let version = u16_at(bytes, 8);
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

fn structure_json(mode: &str, chunk: &[u8]) -> String {
    match mode {
        "file-header" => match header_object(chunk) {
            Some(inner) => format!("{{\"type\":\"file_header\",{inner}}}"),
            None => unknown(),
        },
        "catalog" => catalog_structure(chunk),
        "heap-row" => slotted_structure(chunk),
        "heap-pax" => pax_structure(chunk),
        "btree" => btree_structure(chunk),
        "wal" => format!("{{\"type\":\"wal\",\"frames\":{}}}", wal_frames(chunk).len()),
        "dwb" => format!("{{\"type\":\"dwb\",\"records\":{}}}", dwb_records(chunk).len()),
        "lsm-sstable" => sstable_structure(chunk),
        "lsm-manifest" => manifest_structure(chunk),
        _ => unknown(),
    }
}

fn catalog_structure(chunk: &[u8]) -> String {
    let Some(inner) = header_object(chunk) else {
        return unknown();
    };
    format!(
        "{{\"type\":\"catalog\",{inner},\"next_table_file\":{},\"next_index_file\":{},\
         \"next_trx_id\":{},\"clog_base\":{},\"committed\":{}}}",
        u32_at(chunk, 15),
        u32_at(chunk, 19),
        u64_at(chunk, 23),
        u64_at(chunk, 31),
        u32_at(chunk, 39)
    )
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
        let offset = u16_at(page, 4 + i * 4);
        let length = u16_at(page, 4 + i * 4 + 2);
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

fn pax_structure(page: &[u8]) -> String {
    if !crate::storage::pax::is_pax(page) {
        return unknown();
    }
    format!(
        "{{\"type\":\"pax\",\"num_slots\":{},\"nrows\":{},\"ncols\":{},\"nseg\":{}}}",
        u16_at(page, 4),
        u16_at(page, 4),
        u16_at(page, 6),
        u16_at(page, 8)
    )
}

fn btree_structure(page: &[u8]) -> String {
    let leaf = page.first() == Some(&0);
    let node_type = if leaf { "leaf" } else { "internal" };
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

/// Decodes a single SSTable region on its own, so clicking a region in the
/// console shows just that region's bytes rather than the whole table.
fn sstable_region_decode(kind: &str, chunk: &[u8]) -> (String, Vec<Field>) {
    match kind {
        "footer" => {
            let (filter_offset, filter_size, index_offset, index_size) = if chunk.len() >= 32 {
                (u64_at(chunk, 0), u64_at(chunk, 8), u64_at(chunk, 16), u64_at(chunk, 24))
            } else {
                (0, 0, 0, 0)
            };
            let structure = format!(
                "{{\"type\":\"sstable_footer\",\"filter_offset\":{filter_offset},\
                 \"filter_size\":{filter_size},\"index_offset\":{index_offset},\
                 \"index_size\":{index_size}}}"
            );
            let mut fields = vec![
                field("filter_offset", 0, 8, "offset", "布隆过滤器起始偏移（u64 LE）"),
                field("filter_size", 8, 8, "size", "布隆过滤器长度（u64 LE）"),
                field("index_offset", 16, 8, "offset", "索引块起始偏移（u64 LE）"),
                field("index_size", 24, 8, "size", "索引块长度（u64 LE）"),
                field("magic", 32, 8, "magic", "魔数 SSTBL002"),
            ];
            fields.retain(|f| f.start < chunk.len());
            (structure, fields)
        }
        "data" => (
            "{\"type\":\"sstable_data_block\"}".to_string(),
            vec![field(
                "data_block",
                0,
                chunk.len(),
                "data",
                "数据块：前缀压缩的 key/value 条目 + restart 数组",
            )],
        ),
        "index" => (
            "{\"type\":\"sstable_index_block\"}".to_string(),
            vec![field(
                "index_block",
                0,
                chunk.len(),
                "index",
                "索引块：每个数据块的末键 -> (offset, size)",
            )],
        ),
        "bloom" => (
            "{\"type\":\"sstable_bloom\"}".to_string(),
            vec![field("bloom", 0, chunk.len(), "bloom", "布隆过滤器：位数组 + 哈希个数")],
        ),
        _ => (unknown(), Vec::new()),
    }
}

fn sstable_structure(chunk: &[u8]) -> String {
    let regions = sstable_regions(chunk);
    let blocks = regions.iter().filter(|(kind, _, _)| *kind == "data").count();
    let bloom = regions.iter().find(|(kind, _, _)| *kind == "bloom").map_or(0, |(_, _, len)| *len);
    let index = regions.iter().find(|(kind, _, _)| *kind == "index").map_or(0, |(_, _, len)| *len);
    format!(
        "{{\"type\":\"lsm_sstable\",\"blocks\":{blocks},\"bloom_bytes\":{bloom},\
         \"index_bytes\":{index},\"footer\":{}}}",
        if chunk.len() >= 40 { 40 } else { 0 }
    )
}

fn manifest_structure(chunk: &[u8]) -> String {
    let magic = chunk.get(0..8).map_or(String::new(), |b| String::from_utf8_lossy(b).into_owned());
    let text = std::str::from_utf8(chunk)
        .ok()
        .map_or_else(|| "null".to_string(), json_string);
    format!(
        "{{\"type\":\"lsm_manifest\",\"magic\":{},\"next_sst_no\":{},\"levels\":{},\"text\":{text}}}",
        json_string(&magic),
        u32_at(chunk, 8),
        u32_at(chunk, 12)
    )
}

// -------------------------------------------------------------------- fields

struct Field {
    label: String,
    start: usize,
    len: usize,
    kind: &'static str,
    note: String,
}

fn field(label: &str, start: usize, len: usize, kind: &'static str, note: &str) -> Field {
    Field { label: label.to_string(), start, len, kind, note: note.to_string() }
}

/// Serializes the fields, clamping each range to the chunk and dropping any
/// that would fall entirely outside it.
fn fields_json(chunk_len: usize, fields: &[Field]) -> String {
    let mut out = String::from("[");
    let mut first = true;
    for spec in fields {
        if spec.start >= chunk_len {
            continue;
        }
        let len = spec.len.min(chunk_len - spec.start);
        if len == 0 {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&format!(
            "{{\"label\":{},\"start\":{},\"len\":{},\"kind\":{},\"note\":{}}}",
            json_string(&spec.label),
            spec.start,
            len,
            json_string(spec.kind),
            json_string(&spec.note)
        ));
    }
    out.push(']');
    out
}

fn mode_fields(mode: &str, chunk: &[u8]) -> Vec<Field> {
    match mode {
        "file-header" => {
            if header_object(chunk).is_some() {
                header_fields()
            } else {
                Vec::new()
            }
        }
        "catalog" => catalog_fields(chunk),
        "heap-row" => slotted_fields(chunk),
        "heap-pax" => pax_fields(chunk),
        "btree" => btree_fields(chunk),
        "wal" => wal_fields(chunk),
        "dwb" => dwb_fields(chunk),
        "lsm-sstable" => sstable_fields(chunk),
        "lsm-manifest" => manifest_fields(chunk),
        _ => Vec::new(),
    }
}

fn header_fields() -> Vec<Field> {
    vec![
        field("magic", 0, 8, "magic", "文件魔数"),
        field("format_version", 8, 2, "version", "格式版本"),
        field("file_kind", 10, 1, "kind", "文件类型 (0=堆, 1=索引, 2=目录)"),
        field("page_size", 11, 4, "size", "页大小"),
    ]
}

fn catalog_fields(chunk: &[u8]) -> Vec<Field> {
    let mut out = header_fields();
    out.push(field("next_table_file", 15, 4, "count", "下一个表文件号"));
    out.push(field("next_index_file", 19, 4, "count", "下一个索引文件号"));
    out.push(field("next_trx_id", 23, 8, "id", "下一个事务号"));
    out.push(field("clog_base", 31, 8, "id", "clog 基线"));
    let committed = u32_at(chunk, 39) as usize;
    out.push(field("committed_count", 39, 4, "count", "已提交事务数"));
    let ids_start = 43;
    let ids_len = committed.saturating_mul(8);
    if ids_len > 0 {
        out.push(field("committed_ids", ids_start, ids_len, "body", "已提交事务号"));
    }
    let body_start = ids_start.saturating_add(ids_len);
    if chunk.len() > body_start {
        out.push(field("body", body_start, chunk.len() - body_start, "body", "表/索引/视图等变长节"));
    }
    out
}

fn slotted_fields(page: &[u8]) -> Vec<Field> {
    let mut out = Vec::new();
    let num_slots = u16_at(page, 0) as usize;
    let raw_free_upper = u16_at(page, 2) as usize;
    let free_upper = if raw_free_upper == 0 { PAGE_SIZE } else { raw_free_upper };
    out.push(field("num_slots", 0, 2, "count", "槽数"));
    out.push(field("free_upper", 2, 2, "count", "空闲区上界"));
    out.push(field("slot_directory", 4, num_slots.saturating_mul(4), "directory", "槽目录"));
    for i in 0..num_slots {
        let base = 4 + i * 4;
        let offset = u16_at(page, base) as usize;
        let length = u16_at(page, base + 2) as usize;
        let status = if offset == 0 && length == 0 { "空闲" } else { "在用" };
        out.push(field(
            &format!("slot {i}"),
            base,
            4,
            "slot",
            &format!("槽 {i}: offset={offset}, len={length}, 状态={status}"),
        ));
        if !(offset == 0 && length == 0) {
            out.push(field(&format!("record {i}"), offset, length, "record", &format!("行: slot {i}")));
        }
    }
    let dir_end = 4 + num_slots.saturating_mul(4);
    if free_upper > dir_end {
        out.push(field("free_space", dir_end, free_upper - dir_end, "free", "空闲空间"));
    }
    out
}

fn pax_fields(page: &[u8]) -> Vec<Field> {
    if !crate::storage::pax::is_pax(page) {
        return Vec::new();
    }
    let mut out = vec![
        field("magic", 0, 4, "magic", "PAX3 魔数"),
        field("nrows", 4, 2, "count", "行槽高水位"),
        field("ncols", 6, 2, "count", "列数"),
        field("nseg", 8, 2, "count", "段数 (列数+1)"),
        field("used", 10, 2, "size", "已用字节"),
        field("reserved", 12, 4, "reserved", "保留"),
    ];
    let nseg = u16_at(page, 8) as usize;
    out.push(field("segment_directory", 16, nseg.saturating_mul(4), "directory", "段目录"));
    for i in 0..nseg {
        let base = 16 + i * 4;
        let offset = u16_at(page, base) as usize;
        let length = u16_at(page, base + 2) as usize;
        if i == 0 {
            out.push(field(
                "version_segment",
                offset,
                length,
                "version",
                "版本段: 每行 creator/deleter/next_rid 共 24 字节",
            ));
        } else {
            out.push(field("column_segment", offset, length, "column", &format!("列 {}", i - 1)));
        }
    }
    out
}

fn btree_fields(page: &[u8]) -> Vec<Field> {
    let leaf = page.first() == Some(&0);
    let mut out = vec![
        field("node_type", 0, 1, "node_type", if leaf { "叶子节点" } else { "内部节点" }),
        field("num", 1, 2, "count", "条目数"),
        field("prev", 3, 4, "page", "左兄弟页号"),
        field("next", 7, 4, "page", "右兄弟页号"),
    ];
    let high_key_len = u16_at(page, 11) as usize;
    out.push(field("high_key_len", 11, 2, "count", "高键长度"));
    if high_key_len > 0 {
        out.push(field("high_key", 13, high_key_len + 6, "highkey", "高键 (键 + rid)"));
    }
    let header_len = 13 + if high_key_len > 0 { high_key_len + 6 } else { 0 };
    let count = u16_at(page, 1) as usize;
    let mut total = 0usize;
    for _ in 0..count {
        let key_len = u16_at(page, header_len + total) as usize;
        total += 2 + key_len + 6 + if leaf { 0 } else { 4 };
    }
    out.push(field("entries", header_len, total, "entry", "条目区"));
    for i in 0..count.min(16) {
        let at = entry_offset(page, header_len, i, leaf);
        let size = entry_size(page, at, leaf);
        if at + size > page.len() {
            break;
        }
        let note = entry_note(page, at, i, leaf);
        out.push(field(&format!("entry {i}"), at, size, "entry", &note));
    }
    out
}

fn entry_offset(page: &[u8], header_len: usize, index: usize, leaf: bool) -> usize {
    let mut at = header_len;
    for _ in 0..index {
        at += entry_size(page, at, leaf);
    }
    at
}

fn entry_size(page: &[u8], at: usize, leaf: bool) -> usize {
    let key_len = u16_at(page, at) as usize;
    2 + key_len + 6 + if leaf { 0 } else { 4 }
}

fn entry_note(page: &[u8], at: usize, index: usize, leaf: bool) -> String {
    let key_len = u16_at(page, at) as usize;
    let key = page.get(at + 2..at + 2 + key_len).unwrap_or_default();
    let base = at + 2 + key_len;
    let page_no = u32_at(page, base);
    let slot = u16_at(page, base + 4);
    if leaf {
        format!("条目 {index}: key={} rid=p{page_no}s{slot}", hex_prefix(key))
    } else {
        let child = u32_at(page, base + 6);
        format!("条目 {index}: key={} rid=p{page_no}s{slot} child={child}", hex_prefix(key))
    }
}

fn hex_prefix(bytes: &[u8]) -> String {
    let shown = &bytes[..bytes.len().min(8)];
    let mut out = shown.iter().map(|b| format!("{b:02x}")).collect::<String>();
    if bytes.len() > shown.len() {
        out.push_str("..");
    }
    out
}

fn wal_fields(chunk: &[u8]) -> Vec<Field> {
    let mut out = Vec::new();
    for (pos, len, ty, trx) in wal_frames(chunk) {
        out.push(field("frame_len", pos, 4, "length", "帧长度"));
        out.push(field(
            "frame_type",
            pos + 4,
            1,
            "type",
            match ty {
                1 => "类型: 插入",
                2 => "类型: 删除标记",
                3 => "类型: 提交",
                _ => "类型: 未知",
            },
        ));
        out.push(field("trx_id", pos + 5, 8, "trx", &format!("事务号 {trx}")));
        let payload = len - 9;
        if payload > 0 {
            out.push(field("payload", pos + 13, payload, "payload", "记录负载"));
        }
    }
    out
}

fn dwb_fields(chunk: &[u8]) -> Vec<Field> {
    let mut out = Vec::new();
    for (pos, _total) in dwb_records(chunk) {
        let path_len = u32_at(chunk, pos) as usize;
        out.push(field("path_len", pos, 4, "length", "目标路径长度"));
        if path_len > 0 {
            let path = chunk
                .get(pos + 4..pos + 4 + path_len)
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            out.push(field("path", pos + 4, path_len, "path", &format!("目标路径 {path}")));
        }
        let page_no_at = pos + 4 + path_len;
        out.push(field("page_no", page_no_at, 4, "page_no", "目标页号"));
        out.push(field("page", page_no_at + 4, PAGE_SIZE, "page", "页镜像 (8192 B)"));
    }
    out
}

fn sstable_fields(chunk: &[u8]) -> Vec<Field> {
    let mut out = Vec::new();
    let mut data_index = 0usize;
    for (kind, start, len) in sstable_regions(chunk) {
        let (label, note, field_kind) = match kind {
            "data" => {
                let label = format!("data_block {data_index}");
                let note = format!("数据块 {data_index}");
                data_index += 1;
                (label, note, "data")
            }
            "bloom" => ("bloom".to_string(), "布隆过滤器".to_string(), "bloom"),
            "index" => ("index_block".to_string(), "索引块".to_string(), "index"),
            "footer" => (
                "footer".to_string(),
                "尾部: filter/index 位置 + magic".to_string(),
                "footer",
            ),
            _ => ("region".to_string(), "区域".to_string(), "data"),
        };
        out.push(field(&label, start, len, field_kind, &note));
    }
    if chunk.len() >= 8 {
        out.push(field("magic", chunk.len() - 8, 8, "magic", "魔数 SSTBL002"));
    }
    out
}

fn manifest_fields(chunk: &[u8]) -> Vec<Field> {
    let mut out = vec![field("magic", 0, 8, "magic", "魔数 LSMMF002")];
    if chunk.len() > 8 {
        out.push(field("next_sst_no", 8, 4, "count", "下一个表号"));
        out.push(field("level_count", 12, 4, "count", "层数"));
    }
    if chunk.len() > 16 {
        out.push(field("body", 16, chunk.len() - 16, "body", "各层存活的表号列表"));
    }
    out
}

// ------------------------------------------------------------------ int reads

fn basename(file: &str) -> &str {
    file.rsplit('/').next().unwrap_or(file)
}

fn u16_at(bytes: &[u8], off: usize) -> u16 {
    match bytes.get(off..off + 2) {
        Some(b) => u16::from_le_bytes([b[0], b[1]]),
        None => 0,
    }
}

fn u32_at(bytes: &[u8], off: usize) -> u32 {
    match bytes.get(off..off + 4) {
        Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    }
}

fn u64_at(bytes: &[u8], off: usize) -> u64 {
    match bytes.get(off..off + 8) {
        Some(b) => u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
        None => 0,
    }
}
