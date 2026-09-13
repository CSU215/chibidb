//! PAX and row pages must answer every query identically; these run the same
//! workload against each layout and compare the results.

use chibidb::config::PageLayout;
use chibidb::value::Value;
use chibidb::{Database, ResultSet};

fn kw(layout: PageLayout) -> &'static str {
    match layout {
        PageLayout::Row => "row",
        PageLayout::Pax => "pax",
    }
}

fn rows(rs: &[ResultSet]) -> &[Vec<Value>] {
    match &rs[0] {
        ResultSet::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Runs a mixed OLTP workload (inserts across pages, NULLs, an out-of-line LOB,
/// an index, MVCC update, delete, and reads) and returns every result.
fn scenario(layout: PageLayout) -> Vec<Vec<Vec<Value>>> {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql(&format!(
        "create table t (id int primary key, g int, name char(20), body text, v int) \
         page_layout = {};",
        kw(layout)
    ))
    .unwrap();

    for i in 0..400 {
        let g = if i % 11 == 0 {
            "null".into()
        } else {
            (i % 7).to_string()
        };
        let name = if i % 13 == 0 {
            "null".into()
        } else {
            format!("'n{i}'")
        };
        let v = if i % 17 == 0 {
            "null".into()
        } else {
            (i * 3).to_string()
        };
        db.execute_sql(&format!("insert into t values ({i}, {g}, {name}, 'x', {v});"))
            .unwrap();
    }
    // a value past the inline LOB limit, stored out of line
    db.execute_sql(&format!("insert into t values (9999, 1, 'big', '{}', 1);", "z".repeat(5000)))
        .unwrap();
    db.execute_sql("create index idx_g on t (g);").unwrap();
    db.execute_sql("update t set v = v + 1 where id % 5 = 0;").unwrap();
    db.execute_sql("delete from t where id % 3 = 0;").unwrap();

    let queries = [
        "select id, g, name, v from t order by id;",
        "select id, name, body from t where id = 9999;",
        "select id, v from t where id = 124;",
        "select count(*), sum(v) from t;",
        "select g, sum(v) from t group by g order by g;",
        "select id, v from t where g = 3 order by id;",
        "select id from t where v is null order by id;",
    ];
    let mut collected = Vec::new();
    for sql in queries {
        let rs = db.execute_sql(sql).unwrap();
        collected.push(rows(&rs).to_vec());
    }
    collected
}

#[test]
fn pax_matches_row_on_a_mixed_workload() {
    assert_eq!(scenario(PageLayout::Row), scenario(PageLayout::Pax));
}

/// Single-column aggregates stream the column straight from storage; compare
/// them (and the filtered fallback) across layouts.
#[test]
fn column_native_aggregates_match_row() {
    fn run(layout: PageLayout) -> Vec<Vec<Value>> {
        let db = Database::open_in_memory().unwrap();
        db.execute_sql(&format!(
            "create table t (id int, v int) page_layout = {};",
            kw(layout)
        ))
        .unwrap();
        for i in 0..200 {
            let v = if i % 9 == 0 { "null".into() } else { i.to_string() };
            db.execute_sql(&format!("insert into t values ({i}, {v});")).unwrap();
        }
        let mut out = Vec::new();
        for sql in [
            "select count(*) from t;",
            "select count(v) from t;",
            "select sum(v), avg(v), min(v), max(v) from t;",
            "select sum(v) from t where id > 50;",
        ] {
            let rs = db.execute_sql(sql).unwrap();
            out.extend(rows(&rs).iter().cloned());
        }
        out
    }
    assert_eq!(run(PageLayout::Row), run(PageLayout::Pax));
}

#[test]
fn multi_column_aggregates_match_row() {
    fn run(layout: PageLayout) -> Vec<Vec<Value>> {
        let db = Database::open_in_memory().unwrap();
        db.execute_sql(&format!(
            "create table t (id int, a int, b int, c int) page_layout = {};",
            kw(layout)
        ))
        .unwrap();
        for i in 0..300 {
            let a = if i % 7 == 0 { "null".into() } else { (i % 5).to_string() };
            let b = if i % 11 == 0 { "null".into() } else { (i * 3).to_string() };
            db.execute_sql(&format!("insert into t values ({i}, {a}, {b}, {i});")).unwrap();
        }
        let mut out = Vec::new();
        for sql in [
            "select sum(a), sum(b), sum(c) from t;",
            "select count(*), count(a), avg(a), min(b), max(a) from t;",
            "select sum(a)+sum(b) from t;",
            "select count(*) from t;",
        ] {
            let rs = db.execute_sql(sql).unwrap();
            out.extend(rows(&rs).iter().cloned());
        }
        out
    }
    assert_eq!(run(PageLayout::Row), run(PageLayout::Pax));
}

/// A wide table stresses the column-major scan the layout exists for.
#[test]
fn wide_pax_matches_row() {
    fn run(layout: PageLayout) -> Vec<Vec<Vec<Value>>> {
        let db = Database::open_in_memory().unwrap();
        let cols: Vec<String> = (0..16).map(|i| format!("c{i} int")).collect();
        db.execute_sql(&format!(
            "create table wide (id int, {}) page_layout = {};",
            cols.join(", "),
            kw(layout)
        ))
        .unwrap();
        for i in 0..300 {
            let vals: Vec<String> = (0..16).map(|k| ((i * (k + 1)) % 97).to_string()).collect();
            db.execute_sql(&format!("insert into wide values ({i}, {});", vals.join(", ")))
                .unwrap();
        }
        let mut out = Vec::new();
        for sql in [
            "select sum(c15) from wide;",
            "select id, c0, c7, c15 from wide where id % 29 = 0 order by id;",
            "select sum(c1), sum(c2), sum(c3) from wide;",
        ] {
            let rs = db.execute_sql(sql).unwrap();
            out.push(rows(&rs).to_vec());
        }
        out
    }
    assert_eq!(run(PageLayout::Row), run(PageLayout::Pax));
}

#[test]
fn pax_vacuum_reclaims_deleted_rows() {
    let db = Database::open_in_memory().unwrap();
    db.execute_sql("create table t (id int, v int) page_layout = pax;").unwrap();
    for i in 0..500 {
        db.execute_sql(&format!("insert into t values ({i}, {i});")).unwrap();
    }
    db.execute_sql("delete from t where id % 2 = 0;").unwrap();
    db.execute_sql("vacuum;").unwrap();
    let rs = db.execute_sql("select count(*), sum(v) from t;").unwrap();
    let expected_sum: i64 = (0..500).filter(|i| i % 2 != 0).map(|i| i as i64).sum();
    assert_eq!(rows(&rs), [[Value::Int(250), Value::Int(expected_sum)]]);
}

#[test]
fn pax_tables_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        db.execute_sql("create table t (id int primary key, v int) page_layout = pax;")
            .unwrap();
        for i in 0..300 {
            db.execute_sql(&format!("insert into t values ({i}, {});", i * 2)).unwrap();
        }
    }
    let db = Database::open(dir.path()).unwrap();
    let rs = db.execute_sql("select sum(v), count(*) from t;").unwrap();
    let expected: i64 = (0..300).map(|i| i * 2).sum();
    assert_eq!(rows(&rs), [[Value::Int(expected), Value::Int(300)]]);
}
