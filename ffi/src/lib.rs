//! Minimal C ABI for embedding chibidb (built as a `cdylib`), so non-Rust
//! frontends and the benchmark harness can call the engine in-process.
//!
//! Strings are NUL-terminated UTF-8. A handle comes from [`chibidb_open`] and
//! is released with [`chibidb_close`]; a result string comes from
//! [`chibidb_query`] and is released with [`chibidb_free`].

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, CString, c_char};
use std::path::Path;
use std::sync::Mutex;

use chibidb::config::{Config, ExecutionMode, PageLayout};
use chibidb::{Database, result};

/// Opaque handle to an open database.
pub struct ChibiDb {
    inner: Database,
}

static LAST_ERROR: Mutex<Option<CString>> = Mutex::new(None);

fn set_error(message: &str) {
    if let Ok(mut slot) = LAST_ERROR.lock() {
        *slot = CString::new(message).ok();
    }
}

fn read_str(ptr: *const c_char) -> Result<String, String> {
    if ptr.is_null() {
        return Err("null string".into());
    }
    let text = unsafe { CStr::from_ptr(ptr) };
    text.to_str().map(str::to_owned).map_err(|e| e.to_string())
}

fn into_c_string(text: String) -> *mut c_char {
    match CString::new(text) {
        Ok(value) => value.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Applies open options to `config`: execution mode (`volcano`/`chunk`) and
/// page layout for new tables (`row`/`pax`), any of them separated by `+` or
/// `,` (e.g. `"chunk+pax"`). Unknown tokens are ignored.
fn apply_open_options(config: &mut Config, options: &str) {
    for token in options.split(['+', ',']) {
        let token = token.trim();
        if token.eq_ignore_ascii_case("chunk") {
            config.execution.mode = ExecutionMode::Chunk;
        } else if token.eq_ignore_ascii_case("volcano") {
            config.execution.mode = ExecutionMode::Volcano;
        } else if token.eq_ignore_ascii_case("pax") {
            config.storage.page_layout = PageLayout::Pax;
        } else if token.eq_ignore_ascii_case("row") {
            config.storage.page_layout = PageLayout::Row;
        }
    }
}

/// Opens (or creates) a database directory. `mode` is an open-options string:
/// `"volcano"`/`"chunk"` pick the execution model, `"pax"`/`"row"` the page
/// layout new tables use, joined with `+` (e.g. `"chunk+pax"`); null uses the
/// defaults. Returns null on failure.
#[unsafe(no_mangle)]
pub extern "C" fn chibidb_open(dir: *const c_char, mode: *const c_char) -> *mut ChibiDb {
    let dir = match read_str(dir) {
        Ok(value) => value,
        Err(error) => {
            set_error(&error);
            return std::ptr::null_mut();
        }
    };
    let mut config = Config::default();
    if let Ok(options) = read_str(mode) {
        apply_open_options(&mut config, &options);
    }
    match Database::open_with_config(Path::new(&dir), &config) {
        Ok(inner) => Box::into_raw(Box::new(ChibiDb { inner })),
        Err(error) => {
            set_error(&error.to_string());
            std::ptr::null_mut()
        }
    }
}

/// Executes SQL, discarding any rows. Returns 0 on success, 1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn chibidb_exec(db: *mut ChibiDb, sql: *const c_char) -> i32 {
    if db.is_null() {
        set_error("null database handle");
        return 1;
    }
    let sql = match read_str(sql) {
        Ok(value) => value,
        Err(error) => {
            set_error(&error);
            return 1;
        }
    };
    let db = unsafe { &*db };
    match db.inner.execute_sql(&sql) {
        Ok(_) => 0,
        Err(error) => {
            set_error(&error.to_string());
            1
        }
    }
}

/// Executes SQL and returns the results as a JSON C string (free it with
/// [`chibidb_free`]). On error returns `{"error":"..."}`.
#[unsafe(no_mangle)]
pub extern "C" fn chibidb_query(db: *mut ChibiDb, sql: *const c_char) -> *mut c_char {
    let handle = if db.is_null() {
        set_error("null database handle");
        return into_c_string(result::encode_error("null database handle"));
    } else {
        unsafe { &*db }
    };
    let sql = match read_str(sql) {
        Ok(value) => value,
        Err(error) => {
            set_error(&error);
            return into_c_string(result::encode_error(&error));
        }
    };
    match handle.inner.execute_sql(&sql) {
        Ok(results) => into_c_string(result::encode_results(&results)),
        Err(error) => {
            let message = error.to_string();
            set_error(&message);
            into_c_string(result::encode_error(&message))
        }
    }
}

/// Releases a string returned by [`chibidb_query`].
#[unsafe(no_mangle)]
pub extern "C" fn chibidb_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}

/// Closes a handle from [`chibidb_open`].
#[unsafe(no_mangle)]
pub extern "C" fn chibidb_close(db: *mut ChibiDb) {
    if !db.is_null() {
        drop(unsafe { Box::from_raw(db) });
    }
}

/// The most recent error message, or null. Owned by the library; valid until
/// the next failing call.
#[unsafe(no_mangle)]
pub extern "C" fn chibidb_last_error() -> *const c_char {
    match LAST_ERROR.lock() {
        Ok(slot) => slot.as_ref().map_or(std::ptr::null(), |text| text.as_ptr()),
        Err(_) => std::ptr::null(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_options_select_mode_and_layout() {
        let mut config = Config::default();
        apply_open_options(&mut config, "volcano+pax");
        assert_eq!(config.execution.mode, ExecutionMode::Volcano);
        assert_eq!(config.storage.page_layout, PageLayout::Pax);

        apply_open_options(&mut config, "chunk,row");
        assert_eq!(config.execution.mode, ExecutionMode::Chunk);
        assert_eq!(config.storage.page_layout, PageLayout::Row);

        // unknown tokens leave the defaults alone
        let mut config = Config::default();
        apply_open_options(&mut config, "bogus");
        assert_eq!(config, Config::default());
    }
}
