use chibidb::parser::parse;
use chibidb::value::Value;

fn eval_first(sql: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let stmts = parse(sql).unwrap();
    assert_eq!(stmts.len(), 1, "sql: {sql}");
    match &stmts[0] {
        chibidb::ast::Stmt::Select(s) => match &s.items[0] {
            chibidb::ast::SelectItem::Expr(e) => Ok(chibidb::exec::eval_const(e)?),
            other => panic!("expected expr, got {other:?}"),
        },
        other => panic!("expected select, got {other:?}"),
    }
}

fn val(sql: &str) -> Value {
    eval_first(sql).unwrap()
}

fn err(sql: &str) {
    assert!(eval_first(sql).is_err(), "expected error for: {sql}");
}

#[test]
fn int_arithmetic() {
    assert_eq!(val("select 1+2;"), Value::Int(3));
    assert_eq!(val("select 5-2*3;"), Value::Int(-1));
    assert_eq!(val("select (1+2)*4;"), Value::Int(12));
}

#[test]
fn integer_division_truncates() {
    assert_eq!(val("select 5/2;"), Value::Int(2));
    assert_eq!(val("select -5/2;"), Value::Int(-2));
    assert_eq!(val("select 5/-2;"), Value::Int(-2));
}

#[test]
fn numeric_promotion() {
    assert_eq!(val("select 1+2.5;"), Value::Float(3.5));
    assert_eq!(val("select 5/2.0;"), Value::Float(2.5));
    assert_eq!(val("select 1 < 2.5;"), Value::Bool(true));
    assert_eq!(val("select 2 = 2.0;"), Value::Bool(true));
}

#[test]
fn string_equality() {
    assert_eq!(val("select 'a' < 'b';"), Value::Bool(true));
    assert_eq!(val("select 'a' = 'a';"), Value::Bool(true));
    assert_eq!(val("select 'a' = 'b';"), Value::Bool(false));
}

#[test]
fn logic_operators() {
    assert_eq!(val("select not (1 = 1);"), Value::Bool(false));
    assert_eq!(val("select 1 = 1 and 2 = 2;"), Value::Bool(true));
    assert_eq!(val("select 1 = 1 or 1 = 2;"), Value::Bool(true));
    assert_eq!(val("select not (1 = 1) or 2 = 2;"), Value::Bool(true));
}

#[test]
fn runtime_errors() {
    err("select 1/0;");
    err("select 1.0/0.0;");
    err("select 1/0.0;");
    err("select 9223372036854775807 + 1;");
    err("select -9223372036854775807 - 2;");
    err("select 'a' + 1;");
    err("select 'a' < 1;");
    err("select 1 and 2;");
    err("select not 1;");
    err("select ab;");
}

#[test]
fn value_display() {
    assert_eq!(Value::Int(3).to_string(), "3");
    assert_eq!(Value::Float(2.5).to_string(), "2.5");
    assert_eq!(Value::Float(2.0).to_string(), "2.0");
    assert_eq!(Value::Str("ab".into()).to_string(), "ab");
    assert_eq!(Value::Bool(true).to_string(), "true");
    assert_eq!(Value::Null.to_string(), "NULL");
}

#[test]
fn null_propagates_through_arithmetic_and_comparison() {
    assert_eq!(val("select null;"), Value::Null);
    assert_eq!(val("select null + 1;"), Value::Null);
    assert_eq!(val("select 1 * null;"), Value::Null);
    assert_eq!(val("select -null;"), Value::Null);
    assert_eq!(val("select null = null;"), Value::Null);
    assert_eq!(val("select null < 1;"), Value::Null);
    assert_eq!(val("select null = 'a';"), Value::Null);
    assert_eq!(val("select 'a' <> null;"), Value::Null);
}

#[test]
fn three_valued_logic() {
    assert_eq!(val("select not null;"), Value::Null);
    assert_eq!(val("select null and 1 = 1;"), Value::Null);
    assert_eq!(val("select null and 1 = 2;"), Value::Bool(false));
    assert_eq!(val("select null or 1 = 1;"), Value::Bool(true));
    assert_eq!(val("select null or 1 = 2;"), Value::Null);
    assert_eq!(val("select null and null;"), Value::Null);
    assert_eq!(val("select null or null;"), Value::Null);
}

#[test]
fn null_type_errors_still_error() {
    err("select null and 'a';");
    err("select null + 'a';");
}

#[test]
fn like_matching() {
    assert_eq!(val("select 'abc' like 'abc';"), Value::Bool(true));
    assert_eq!(val("select 'abc' like 'a%';"), Value::Bool(true));
    assert_eq!(val("select 'abc' like '%c';"), Value::Bool(true));
    assert_eq!(val("select 'abc' like '%b%';"), Value::Bool(true));
    assert_eq!(val("select 'abc' like 'a_c';"), Value::Bool(true));
    assert_eq!(val("select 'abc' like '_bc';"), Value::Bool(true));
    assert_eq!(val("select 'abc' like '%%';"), Value::Bool(true));
    assert_eq!(val("select 'abc' like '%';"), Value::Bool(true));
    assert_eq!(val("select '' like '%';"), Value::Bool(true));
    assert_eq!(val("select 'abc' like 'a_';"), Value::Bool(false));
    assert_eq!(val("select '' like '_';"), Value::Bool(false));
    assert_eq!(val("select 'abc' like 'ABC';"), Value::Bool(false), "case-sensitive");
    assert_eq!(val("select 'ba' like '%a%b%';"), Value::Bool(false));
    assert_eq!(val("select 'a%b' like 'a%%b';"), Value::Bool(true), "% is literal-free in text");
}

#[test]
fn like_negation_and_null() {
    assert_eq!(val("select 'abc' not like 'x%';"), Value::Bool(true));
    assert_eq!(val("select 'abc' not like 'a%';"), Value::Bool(false));
    assert_eq!(val("select null like 'a';"), Value::Null);
    assert_eq!(val("select 'a' like null;"), Value::Null);
    assert_eq!(val("select null not like 'a';"), Value::Null);
    err("select 1 like 'a';");
    err("select 'a' like 1;");
}

#[test]
fn modulo_operator() {
    assert_eq!(val("select 5 % 2;"), Value::Int(1));
    assert_eq!(val("select -5 % 2;"), Value::Int(-1));
    assert_eq!(val("select 5 % 2.0;"), Value::Float(1.0));
    assert_eq!(val("select 5.5 % 2;"), Value::Float(1.5));
    err("select 5 % 0;");
    err("select 5 % 0.0;");
}

#[test]
fn string_functions() {
    assert_eq!(val("select concat('a', 'b', 'c');"), Value::Str("abc".into()));
    assert_eq!(val("select concat('n=', 42);"), Value::Str("n=42".into()));
    assert_eq!(val("select upper('aBc');"), Value::Str("ABC".into()));
    assert_eq!(val("select lower('AbC');"), Value::Str("abc".into()));
    assert_eq!(val("select length('hello');"), Value::Int(5));
    assert_eq!(val("select length('');"), Value::Int(0));
    assert_eq!(val("select length('héllo');"), Value::Int(5), "counts chars not bytes");
    assert_eq!(val("select substring('hello', 2, 3);"), Value::Str("ell".into()));
    assert_eq!(val("select substring('hello', 2);"), Value::Str("ello".into()));
    assert_eq!(val("select substring('hello', 10);"), Value::Str("".into()));
    assert_eq!(val("select substr('hello', 1, 1);"), Value::Str("h".into()));
    assert_eq!(val("select upper(substring(lower('ABC'), 2));"), Value::Str("BC".into()));
}

#[test]
fn string_function_null_and_errors() {
    assert_eq!(val("select concat('a', null);"), Value::Null);
    assert_eq!(val("select upper(null);"), Value::Null);
    assert_eq!(val("select length(null);"), Value::Null);
    assert_eq!(val("select substring(null, 1);"), Value::Null);
    assert_eq!(val("select substring('a', null);"), Value::Null);
    assert_eq!(val("select substring('a', 1, null);"), Value::Null);
    err("select upper(1);");
    err("select length(1);");
    err("select substring('a', 0);");
    err("select substring('a', 1, -1);");
    err("select upper('a', 'b');");
    err("select substring('a');");
    err("select nope(1);");
}

#[test]
fn is_null_evaluation() {
    assert_eq!(val("select null is null;"), Value::Bool(true));
    assert_eq!(val("select null is not null;"), Value::Bool(false));
    assert_eq!(val("select 1 is null;"), Value::Bool(false));
    assert_eq!(val("select 'a' is not null;"), Value::Bool(true));
}
