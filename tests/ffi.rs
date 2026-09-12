//! Exercises the C ABI directly (the entry points are callable from Rust too).

use std::ffi::{CStr, CString, c_char};

use chibidb::ffi::{chibidb_close, chibidb_exec, chibidb_free, chibidb_last_error, chibidb_open, chibidb_query};

fn c(text: &str) -> CString {
    CString::new(text).unwrap()
}

unsafe fn take(ptr: *mut c_char) -> String {
    let text = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_owned();
    chibidb_free(ptr);
    text
}

#[test]
fn ffi_round_trips_sql_and_reports_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = c(dir.path().to_str().unwrap());
    let mode = c("chunk");
    let db = chibidb_open(path.as_ptr(), mode.as_ptr());
    assert!(!db.is_null());

    assert_eq!(chibidb_exec(db, c("create table t (id int, v int);").as_ptr()), 0);
    assert_eq!(chibidb_exec(db, c("insert into t values (1,10),(2,20);").as_ptr()), 0);

    let json = unsafe { take(chibidb_query(db, c("select sum(v) from t;").as_ptr())) };
    assert!(json.contains("\"results\""), "{json}");
    assert!(json.contains("30"), "{json}");

    assert_eq!(chibidb_exec(db, c("select from nowhere;").as_ptr()), 1);
    assert!(!chibidb_last_error().is_null());

    chibidb_close(db);
}

#[test]
fn ffi_open_rejects_a_null_path() {
    let db = chibidb_open(std::ptr::null(), std::ptr::null());
    assert!(db.is_null());
    assert!(!chibidb_last_error().is_null());
}
