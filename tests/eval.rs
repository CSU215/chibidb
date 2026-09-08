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
fn is_null_evaluation() {
    assert_eq!(val("select null is null;"), Value::Bool(true));
    assert_eq!(val("select null is not null;"), Value::Bool(false));
    assert_eq!(val("select 1 is null;"), Value::Bool(false));
    assert_eq!(val("select 'a' is not null;"), Value::Bool(true));
}
