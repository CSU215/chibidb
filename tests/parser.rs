use chibidb::parser::parse;

fn single_exprs(sql: &str) -> Vec<String> {
    let stmts = parse(sql).unwrap();
    assert_eq!(stmts.len(), 1, "sql: {sql}");
    match stmts.into_iter().next().unwrap() {
        chibidb::ast::Stmt::Select(s) => s.exprs.iter().map(|e| e.to_string()).collect(),
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
