//! Differential tests: the chunk (columnar) path must produce exactly the same
//! results as the volcano (row) path for every statement shape it covers.

use chibidb::config::{Config, ExecutionMode};
use chibidb::{Database, ResultSet};

fn run(mode: ExecutionMode, script: &str) -> Vec<ResultSet> {
    let mut config = Config::default();
    config.execution.mode = mode;
    let db = Database::open_in_memory_with_config(&config).unwrap();
    db.execute_sql(script).unwrap()
}

/// Runs `script` under both modes and asserts identical output.
fn assert_same(script: &str) {
    let volcano = run(ExecutionMode::Volcano, script);
    let chunk = run(ExecutionMode::Chunk, script);
    assert_eq!(volcano, chunk, "chunk path diverged for:\n{script}");
}

#[test]
fn scan_and_projection_match() {
    assert_same(
        "create table t (id int, name char(10), score float);\
         insert into t values (1,'a',1.5),(2,'b',2.0),(3,null,3.25);\
         select * from t;\
         select id, score from t;\
         select id + 1 as x, score * 2.0 as y from t;\
         select upper(name), length(name) from t;",
    );
}

#[test]
fn filters_match() {
    assert_same(
        "create table t (id int, v int);\
         insert into t values (1,10),(2,20),(3,30),(4,40);\
         select * from t where id > 2;\
         select * from t where v >= 20 and v < 40;\
         select id from t where id in (1,3);\
         select * from t where id is not null;",
    );
}

#[test]
fn null_predicates_match() {
    assert_same(
        "create table t (id int, v int);\
         insert into t values (1,10),(2,null),(3,30);\
         select * from t where v = 10;\
         select * from t where v is null;\
         select * from t where not (v = 10);\
         select * from t where v <> 10;",
    );
}

#[test]
fn index_scans_match() {
    assert_same(
        "create table t (id int primary key, v int);\
         insert into t values (1,10),(2,20),(3,30);\
         select * from t where id = 2;\
         select * from t where id >= 1 and id < 3;\
         select * from t where id > 1 order by id desc;\
         select v from t where id = 3;",
    );
}

#[test]
fn aggregates_ordering_and_distinct_match() {
    assert_same(
        "create table t (id int, v int);\
         insert into t values (1,10),(2,20),(2,20),(3,30);\
         select count(*), sum(v), avg(v), min(v), max(v) from t;\
         select distinct v from t order by v;\
         select v, count(*) from t group by v order by v;\
         select * from t order by v desc limit 2;\
         select v from t where v >= 20 order by v;",
    );
}

#[test]
fn joins_match() {
    assert_same(
        "create table a (id int, x int);\
         create table b (id int, y int);\
         insert into a values (1,10),(2,20),(3,30),(null,40);\
         insert into b values (1,100),(1,101),(2,200),(null,300);\
         select a.id, x, y from a join b on a.id = b.id;\
         select a.id, x, y from a join b on a.id = b.id order by a.id, y;\
         select a.id, y from a left join b on a.id = b.id;\
         select a.id, y from a right join b on a.id = b.id;\
         select a.id, x, y from a join b on a.id = b.id and a.x < b.y;\
         select a.id, x, y from a join b on a.id = b.id and a.x = b.y;",
    );
    assert_same(
        "create table a (id int, x int);\
         create table b (id int, y int);\
         insert into a values (1,1),(1,2),(2,1);\
         insert into b values (1,1),(1,2),(2,2);\
         select a.id from a join b on a.id = b.id and a.x = b.y;\
         select a.id, b.id from a join b on a.id = b.id and a.x = b.y;",
    );
}

#[test]
fn dml_then_scan_matches() {
    assert_same(
        "create table t (id int, v int);\
         insert into t values (1,10),(2,20),(3,30);\
         update t set v = v + 5 where id = 1;\
         delete from t where id = 2;\
         select * from t;\
         select sum(v) from t;",
    );
}

#[test]
fn constant_select_matches() {
    assert_same("select 1 + 2;\
                 select 'x';");
}

#[test]
fn lsm_engine_matches() {
    assert_same(
        "create table t (id int, v int) engine = lsm;\
         insert into t values (1,10),(2,20),(3,30);\
         select * from t where v >= 20;\
         select sum(v) from t;",
    );
}

#[test]
fn ordering_matches() {
    assert_same(
        "create table t (id int, v int);\
         insert into t values (1,3),(2,1),(3,null),(4,2),(5,1);\
         select id from t order by v;\
         select id from t order by v desc;\
         select id from t order by v asc, id desc;\
         select id from t order by v limit 2;\
         select count(*) as c, v from t group by v order by v;",
    );
}

#[test]
fn grouped_aggregates_match() {
    assert_same(
        "create table t (w int, v int);\
         insert into t values (1,10),(2,20),(1,30),(null,40),(2,50),(null,60);\
         select w, count(*) from t group by w;\
         select w, sum(v), avg(v), min(v), max(v) from t group by w;\
         select w, count(v) from t group by w;\
         select v, count(*) from t group by w;\
         select w from t group by w;\
         select w, sum(v) from t where v >= 20 group by w;",
    );
    assert_same(
        "create table t (a int, b int);\
         insert into t values (1,1),(1,2),(2,1),(2,2),(3,3);\
         select a, b, count(*) from t group by a, b;\
         select a, count(*) from t group by a;",
    );
}

#[test]
fn vectorized_predicates_match() {
    assert_same(
        "create table t (id int, a int, b int, score float, name char(4));\
         insert into t values (1,1,1,1.5,'x'),(2,1,2,2.5,'y'),(3,null,3,null,null);\
         select * from t where a = b;\
         select * from t where a <> b;\
         select * from t where score > 1.5;\
         select * from t where id = 1 or id = 3;\
         select * from t where (id >= 1 and score < 3.0) or name = 'y';\
         select * from t where a is null;\
         select * from t where a is not null;\
         select * from t where name = 'x';\
         select * from t where a + b > 2;",
    );
}

#[test]
fn aggregate_edge_cases_match() {
    assert_same(
        "create table t (id int, v int, f float, name char(4));\
         insert into t values (1,null,null,'b'),(2,10,1.5,'a'),(3,20,2.5,null);\
         select sum(v) from t;\
         select count(v) from t;\
         select avg(v), avg(f) from t;\
         select min(name), max(name) from t;\
         select sum(v) from t where v > 10;\
         select min(f), max(f) from t;",
    );
    // empty input still yields one aggregate row of NULLs / zero count
    assert_same(
        "create table t (v int);\
         select count(*), sum(v), avg(v), min(v), max(v) from t;",
    );
}

#[test]
fn char_and_text_columns_match() {
    assert_same(
        "create table t (id int, name char(8), body text);\
         insert into t values (1,'alice','hello world'),(2,'bob','x');\
         select name, body from t where name = 'alice';\
         select concat(name, '!') from t;",
    );
}
