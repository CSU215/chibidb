use chibidb::ast::{DataType, SelectItem, Stmt};
use chibidb::parser::parse;

fn single_exprs(sql: &str) -> Vec<String> {
    let stmts = parse(sql).unwrap();
    assert_eq!(stmts.len(), 1, "sql: {sql}");
    match stmts.into_iter().next().unwrap() {
        Stmt::Select(s) => s
            .items
            .iter()
            .map(|it| match it {
                SelectItem::Star => "*".to_string(),
                SelectItem::Expr(e) => e.to_string(),
            })
            .collect(),
        other => panic!("expected select, got {other:?}"),
    }
}

fn err(sql: &str) {
    assert!(parse(sql).is_err(), "expected error for: {sql}");
}

#[test]
fn parses_select_literal() {
    assert_eq!(single_exprs("select 1;"), ["1"]);
}

#[test]
fn parses_comma_separated_exprs() {
    assert_eq!(single_exprs("select 1, 2.5, 'ab';"), ["1", "2.5", "'ab'"]);
}

#[test]
fn select_keyword_is_case_insensitive() {
    assert_eq!(single_exprs("SELECT 1;"), ["1"]);
}

#[test]
fn mul_binds_tighter_than_add() {
    assert_eq!(single_exprs("select 1+2*3;"), ["(+ 1 (* 2 3))"]);
}

#[test]
fn subtraction_is_left_associative() {
    assert_eq!(single_exprs("select 1-2-3;"), ["(- (- 1 2) 3)"]);
}

#[test]
fn parens_override_precedence() {
    assert_eq!(single_exprs("select (1+2)*3;"), ["(* (+ 1 2) 3)"]);
}

#[test]
fn unary_minus() {
    assert_eq!(single_exprs("select -1, -(1+2);"), ["(- 1)", "(- (+ 1 2))"]);
}

#[test]
fn unary_plus_is_identity() {
    assert_eq!(single_exprs("select +1;"), ["1"]);
}

#[test]
fn parses_column_ref() {
    assert_eq!(single_exprs("select ab_1;"), ["ab_1"]);
}

#[test]
fn parses_multiple_statements() {
    let stmts = parse("select 1; select 2;").unwrap();
    assert_eq!(stmts.len(), 2);
}

#[test]
fn trailing_semicolon_optional_and_empty_statements_skipped() {
    assert_eq!(single_exprs("select 1"), ["1"]);
    let stmts = parse(";;").unwrap();
    assert_eq!(stmts.len(), 0);
}

#[test]
fn syntax_errors() {
    err("select;");
    err("select 1 +;");
    err("select 1 2;");
    err("foo;");
    err("select )");
    err("select (1");
}

#[test]
fn parses_comparisons() {
    assert_eq!(single_exprs("select 1 = 2;"), ["(= 1 2)"]);
    assert_eq!(single_exprs("select 1 <> 2;"), ["(<> 1 2)"]);
    assert_eq!(single_exprs("select 1 != 2;"), ["(<> 1 2)"], "!= normalizes to <>");
    assert_eq!(single_exprs("select 1 < 2, 2 <= 2, 3 > 2, 3 >= 3;"),
        ["(< 1 2)", "(<= 2 2)", "(> 3 2)", "(>= 3 3)"]);
}

#[test]
fn comparison_is_looser_than_additive() {
    assert_eq!(single_exprs("select 1+1 = 2;"), ["(= (+ 1 1) 2)"]);
    assert_eq!(single_exprs("select 1 < 2+3;"), ["(< 1 (+ 2 3))"]);
}

#[test]
fn comparisons_are_not_chainable() {
    err("select 1 < 2 < 3;");
}

#[test]
fn parses_and_or_not() {
    assert_eq!(single_exprs("select 1 = 1 and 2 = 2;"), ["(and (= 1 1) (= 2 2))"]);
    assert_eq!(single_exprs("select 1 or 0;"), ["(or 1 0)"]);
    assert_eq!(single_exprs("select not 1;"), ["(not 1)"]);
    assert_eq!(single_exprs("select not not 1;"), ["(not (not 1))"]);
    assert_eq!(single_exprs("select 1 AND 2;"), ["(and 1 2)"]);
}

#[test]
fn logical_precedence() {
    // and binds tighter than or
    assert_eq!(single_exprs("select 1 or 0 and 0;"), ["(or 1 (and 0 0))"]);
    // not binds looser than comparison
    assert_eq!(single_exprs("select not 1 = 1;"), ["(not (= 1 1))"]);
    // left associativity
    assert_eq!(single_exprs("select 1 and 0 and 1;"), ["(and (and 1 0) 1)"]);
}

fn create_table(sql: &str) -> chibidb::ast::CreateTableStmt {
    let stmts = parse(sql).unwrap();
    assert_eq!(stmts.len(), 1, "sql: {sql}");
    match stmts.into_iter().next().unwrap() {
        Stmt::CreateTable(c) => c,
        _ => panic!("expected create table for: {sql}"),
    }
}

#[test]
fn parses_create_table() {
    let c = create_table("create table student (id int, name char(10), score float);");
    assert_eq!(c.name, "student");
    assert_eq!(c.columns.len(), 3);
    assert_eq!(c.columns[0].name, "id");
    assert_eq!(c.columns[0].dtype, DataType::Int);
    assert_eq!(c.columns[1].name, "name");
    assert_eq!(c.columns[1].dtype, DataType::Char(10));
    assert_eq!(c.columns[2].name, "score");
    assert_eq!(c.columns[2].dtype, DataType::Float);
}

#[test]
fn create_table_is_case_insensitive() {
    let c = create_table("CREATE TABLE T (ID INT);");
    assert_eq!(c.name, "T");
    assert_eq!(c.columns[0].dtype, DataType::Int);
}

#[test]
fn create_table_syntax_errors() {
    err("create t (id int);");
    err("create table (id int);");
    err("create table t ();");
    err("create table t (id int,);");
    err("create table t (id int");
    err("create table t (id);");
    err("create table t (s char);");
    err("create table t (s char(0));");
    err("create table t (a bool);");
    err("create table t (id int,);");
}

fn insert(sql: &str) -> chibidb::ast::InsertStmt {
    let stmts = parse(sql).unwrap();
    assert_eq!(stmts.len(), 1, "sql: {sql}");
    match stmts.into_iter().next().unwrap() {
        Stmt::Insert(i) => i,
        other => panic!("expected insert, got {other:?}"),
    }
}

#[test]
fn parses_insert() {
    let i = insert("insert into student values (1, 'alice', 95.5);");
    assert_eq!(i.table, "student");
    assert_eq!(i.rows.len(), 1);
    let row: Vec<String> = i.rows[0].iter().map(|e| e.to_string()).collect();
    assert_eq!(row, ["1", "'alice'", "95.5"]);
}

#[test]
fn parses_multi_row_insert() {
    let i = insert("insert into t values (1), (2, 3.5);");
    assert_eq!(i.table, "t");
    assert_eq!(i.rows.len(), 2);
    assert_eq!(i.rows[0][0].to_string(), "1");
    assert_eq!(i.rows[1][0].to_string(), "2");
    assert_eq!(i.rows[1][1].to_string(), "3.5");
}

#[test]
fn parses_negative_literals() {
    let i = insert("insert into t values (-1, -2.5);");
    assert_eq!(i.rows[0][0].to_string(), "-1");
    assert_eq!(i.rows[0][1].to_string(), "-2.5");
}

#[test]
fn insert_is_case_insensitive() {
    let i = insert("INSERT INTO T VALUES (1);");
    assert_eq!(i.table, "T");
}

#[test]
fn insert_syntax_errors() {
    err("insert t values (1);");
    err("insert into values (1);");
    err("insert into t (1);");
    err("insert into t values ();");
    err("insert into t values (1,);");
    err("insert into t values (1); extra");
    err("insert into t values (1, 2))");
}

fn select(sql: &str) -> chibidb::ast::SelectStmt {
    let stmts = parse(sql).unwrap();
    assert_eq!(stmts.len(), 1, "sql: {sql}");
    match stmts.into_iter().next().unwrap() {
        Stmt::Select(s) => s,
        other => panic!("expected select, got {other:?}"),
    }
}

#[test]
fn parses_select_from_where() {
    let s = select("select id, name from student where id = 1;");
    assert_eq!(s.items.len(), 2);
    assert!(matches!(&s.items[0], SelectItem::Expr(e) if e.to_string() == "id"));
    assert!(matches!(&s.items[1], SelectItem::Expr(e) if e.to_string() == "name"));
    let from = s.from.as_ref().unwrap();
    assert_eq!(from.name, "student");
    assert_eq!(from.alias, None);
    assert_eq!(s.selection.as_ref().unwrap().to_string(), "(= id 1)");
}

#[test]
fn parses_star() {
    let s = select("select * from t;");
    assert_eq!(s.items, [SelectItem::Star]);
    assert_eq!(s.from.as_ref().unwrap().name, "t");
    let s = select("select *, id from t;");
    assert_eq!(s.items.len(), 2);
}

#[test]
fn parses_table_alias() {
    let s = select("select 1 from t as a;");
    assert_eq!(s.from.as_ref().unwrap().alias.as_deref(), Some("a"));
    let s = select("select 1 from t a;");
    assert_eq!(s.from.as_ref().unwrap().alias.as_deref(), Some("a"));
}

#[test]
fn select_without_from_has_no_selection() {
    let s = select("select 1;");
    assert_eq!(s.from, None);
    assert_eq!(s.selection, None);
    let s = select("select 1 from t where 1 and 2;");
    assert_eq!(s.selection.as_ref().unwrap().to_string(), "(and 1 2)");
}

#[test]
fn keywords_in_select_are_case_insensitive() {
    let s = select("SELECT * FROM t WHERE 1;");
    assert_eq!(s.from.as_ref().unwrap().name, "t");
    assert!(s.selection.is_some());
}

#[test]
fn select_from_syntax_errors() {
    err("select * from;");
    err("select 1 from;");
    err("select * from t where;");
    err("select * from t where 1 =;");
}

#[test]
fn parses_delete() {
    let stmts = parse("delete from t;").unwrap();
    match &stmts[0] {
        Stmt::Delete(d) => {
            assert_eq!(d.table, "t");
            assert_eq!(d.selection, None);
        }
        other => panic!("expected delete, got {other:?}"),
    }
    let stmts = parse("delete from t where id = 1 or name = 'x';").unwrap();
    match &stmts[0] {
        Stmt::Delete(d) => {
            assert_eq!(d.table, "t");
            assert_eq!(d.selection.as_ref().unwrap().to_string(), "(or (= id 1) (= name 'x'))");
        }
        other => panic!("expected delete, got {other:?}"),
    }
}

#[test]
fn parses_update() {
    let stmts = parse("update t set score = 100;").unwrap();
    match &stmts[0] {
        Stmt::Update(u) => {
            assert_eq!(u.table, "t");
            assert_eq!(u.assignments.len(), 1);
            assert_eq!(u.assignments[0].0, "score");
            assert_eq!(u.assignments[0].1.to_string(), "100");
            assert_eq!(u.selection, None);
        }
        other => panic!("expected update, got {other:?}"),
    }
    let stmts = parse("update t set a = a + 1, b = 'x' where id = 3;").unwrap();
    match &stmts[0] {
        Stmt::Update(u) => {
            assert_eq!(u.assignments.len(), 2);
            assert_eq!(u.assignments[0].1.to_string(), "(+ a 1)");
            assert_eq!(u.assignments[1].1.to_string(), "'x'");
            assert_eq!(u.selection.as_ref().unwrap().to_string(), "(= id 3)");
        }
        other => panic!("expected update, got {other:?}"),
    }
}

#[test]
fn delete_update_syntax_errors() {
    err("delete t;");
    err("delete from;");
    err("delete from t where;");
    err("update t;");
    err("update t set;");
    err("update t set a;");
    err("update t set a =;");
    err("update t set a = 1,;");
}
