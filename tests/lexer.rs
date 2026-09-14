use chaoticdb::sql::lexer::{Punct, TokenKind, lex};

fn kinds(src: &str) -> Vec<TokenKind> {
    lex(src).unwrap().into_iter().map(|t| t.kind).collect()
}

#[test]
fn tokenizes_integers_and_positions() {
    let toks = lex("1 23 456").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![TokenKind::Int(1), TokenKind::Int(23), TokenKind::Int(456)]
    );
    let toks = lex("  1, 23").unwrap();
    assert_eq!((toks[0].pos, toks[1].pos, toks[2].pos), (2, 3, 5));
}

#[test]
fn tokenizes_punctuation() {
    assert_eq!(
        kinds("(1,2);"),
        vec![
            TokenKind::Punct(Punct::LParen),
            TokenKind::Int(1),
            TokenKind::Punct(Punct::Comma),
            TokenKind::Int(2),
            TokenKind::Punct(Punct::RParen),
            TokenKind::Punct(Punct::Semicolon),
        ]
    );
}

#[test]
fn tokenizes_arithmetic_and_comparison_operators() {
    assert_eq!(
        kinds("+ - * / %"),
        vec![
            TokenKind::Punct(Punct::Plus),
            TokenKind::Punct(Punct::Minus),
            TokenKind::Punct(Punct::Star),
            TokenKind::Punct(Punct::Slash),
            TokenKind::Punct(Punct::Percent),
        ]
    );
    assert_eq!(
        kinds("= <> != < <= > >="),
        vec![
            TokenKind::Punct(Punct::Eq),
            TokenKind::Punct(Punct::NotEq),
            TokenKind::Punct(Punct::NotEq),
            TokenKind::Punct(Punct::Lt),
            TokenKind::Punct(Punct::Le),
            TokenKind::Punct(Punct::Gt),
            TokenKind::Punct(Punct::Ge),
        ]
    );
}

#[test]
fn dot_is_punct() {
    assert_eq!(
        kinds("a.b"),
        vec![
            TokenKind::Ident("a".into()),
            TokenKind::Punct(Punct::Dot),
            TokenKind::Ident("b".into()),
        ]
    );
}

#[test]
fn tokenizes_floats_and_scientific_notation() {
    assert_eq!(kinds("1.5 0.25"), vec![TokenKind::Float(1.5), TokenKind::Float(0.25)]);
    assert_eq!(
        kinds("1e5 1.5e-3 2E+4"),
        vec![TokenKind::Float(1e5), TokenKind::Float(1.5e-3), TokenKind::Float(2e4)]
    );
    let toks = lex("1e5 1.5e-3 2E+4").unwrap();
    assert_eq!((toks[0].pos, toks[1].pos, toks[2].pos), (0, 4, 11));
}

#[test]
fn decimal_point_needs_a_following_digit() {
    // `1.` is integer 1 followed by a dot, not a malformed float.
    assert_eq!(kinds("1."), vec![TokenKind::Int(1), TokenKind::Punct(Punct::Dot)]);
    // a bare `e` after a number is the start of an identifier
    assert_eq!(kinds("1example"), vec![TokenKind::Int(1), TokenKind::Ident("example".into())]);
}

#[test]
fn tokenizes_strings_with_escapes() {
    assert_eq!(kinds("'abc' ''"), vec![TokenKind::Str("abc".into()), TokenKind::Str("".into())]);
    assert_eq!(
        kinds(r"'a\'b' 'a''b' 'a\nb' 'a\\b'"),
        vec![
            TokenKind::Str("a'b".into()),
            TokenKind::Str("a'b".into()),
            TokenKind::Str("a\nb".into()),
            TokenKind::Str("a\\b".into()),
        ]
    );
    let toks = lex("'abc' ''").unwrap();
    assert_eq!((toks[0].pos, toks[1].pos), (0, 6));
}

#[test]
fn tokenizes_identifiers() {
    assert_eq!(
        kinds("select from_1 _abc123"),
        vec![
            TokenKind::Ident("select".into()),
            TokenKind::Ident("from_1".into()),
            TokenKind::Ident("_abc123".into()),
        ]
    );
    assert_eq!(kinds("t1 1"), vec![TokenKind::Ident("t1".into()), TokenKind::Int(1)]);
}

#[test]
fn tokenizes_backtick_identifiers() {
    assert_eq!(
        kinds("`select` `a b` `we``ird`"),
        vec![
            TokenKind::Ident("select".into()),
            TokenKind::Ident("a b".into()),
            TokenKind::Ident("we`ird".into()),
        ]
    );
    let toks = lex("`select` `a b`").unwrap();
    assert_eq!(toks[1].pos, 9);
}

#[test]
fn skips_line_comments() {
    assert_eq!(kinds("1 -- trailing\n2 -- to eof"), vec![TokenKind::Int(1), TokenKind::Int(2)]);
    assert_eq!(kinds("1 -- to eof"), vec![TokenKind::Int(1)]);
}

#[test]
fn single_minus_is_still_minus() {
    assert_eq!(
        kinds("1-2"),
        vec![TokenKind::Int(1), TokenKind::Punct(Punct::Minus), TokenKind::Int(2)]
    );
}

#[test]
fn empty_input_yields_no_tokens() {
    assert!(lex("").unwrap().is_empty());
    assert!(lex(" \t\r\n").unwrap().is_empty());
}

#[test]
fn reports_lexical_errors() {
    assert!(lex("1 @").is_err());
    assert!(lex("'abc").is_err());
    assert!(lex("'a\\").is_err());
    assert!(lex("`abc").is_err());
    assert!(lex("!").is_err());
    assert!(lex("99999999999999999999999").is_err());
}

#[test]
fn error_offsets_point_at_the_problem() {
    match lex("1 @").unwrap_err() {
        chaoticdb::Error::Syntax { pos, .. } => assert_eq!(pos, Some(2)),
        other => panic!("expected syntax error, got {other:?}"),
    }
}
