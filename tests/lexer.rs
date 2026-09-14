use chibidb::lexer::{Punct, TokenKind, lex};

#[test]
fn tokenizes_integers() {
    let toks = lex("1 23 456").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![TokenKind::Int(1), TokenKind::Int(23), TokenKind::Int(456)]
    );
}

#[test]
fn token_positions_are_byte_offsets() {
    let toks = lex("  1, 23").unwrap();
    assert_eq!(toks[0].pos, 2);
    assert_eq!(toks[1].pos, 3);
    assert_eq!(toks[2].pos, 5);
}

#[test]
fn tokenizes_punctuation() {
    let toks = lex("(1,2);").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
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
fn errors_on_unknown_char() {
    assert!(lex("1 @").is_err());
}

#[test]
fn lexes_dot_as_punct() {
    let toks = lex("a.b").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![
            TokenKind::Ident("a".into()),
            TokenKind::Punct(Punct::Dot),
            TokenKind::Ident("b".into())
        ]
    );
}

#[test]
fn tokenizes_floats() {
    let toks = lex("1.5 0.25").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![TokenKind::Float(1.5), TokenKind::Float(0.25)]
    );
    assert_eq!(toks[0].pos, 0);
    assert_eq!(toks[1].pos, 4);
}

#[test]
fn tokenizes_scientific_notation() {
    let toks = lex("1e5 1.5e-3 2E+4").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![
            TokenKind::Float(1e5),
            TokenKind::Float(1.5e-3),
            TokenKind::Float(2e4),
        ]
    );
    assert_eq!(toks[0].pos, 0);
    assert_eq!(toks[1].pos, 4);
    assert_eq!(toks[2].pos, 11);
}

#[test]
fn exponent_requires_digits() {
    let toks = lex("1example").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![TokenKind::Int(1), TokenKind::Ident("example".into())]
    );
}

#[test]
fn tokenizes_strings() {
    let toks = lex("'abc' ''").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![TokenKind::Str("abc".into()), TokenKind::Str("".into())]
    );
    assert_eq!(toks[0].pos, 0);
    assert_eq!(toks[1].pos, 6);
}

#[test]
fn errors_on_unterminated_string() {
    assert!(lex("'abc").is_err());
}

#[test]
fn string_backslash_escapes() {
    let toks = lex(r"'a\'b' 'a''b' 'a\nb' 'a\\b'").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![
            TokenKind::Str("a'b".into()),
            TokenKind::Str("a'b".into()),
            TokenKind::Str("a\nb".into()),
            TokenKind::Str("a\\b".into()),
        ]
    );
}

#[test]
fn errors_on_dangling_escape() {
    assert!(lex("'a\\").is_err());
}

#[test]
fn tokenizes_identifiers() {
    let toks = lex("select from_1 _abc123").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![
            TokenKind::Ident("select".into()),
            TokenKind::Ident("from_1".into()),
            TokenKind::Ident("_abc123".into()),
        ]
    );
}

#[test]
fn tokenizes_backtick_identifiers() {
    let toks = lex("`select` `a b` `we``ird`").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![
            TokenKind::Ident("select".into()),
            TokenKind::Ident("a b".into()),
            TokenKind::Ident("we`ird".into()),
        ]
    );
    assert_eq!(toks[0].pos, 0);
    assert_eq!(toks[1].pos, 9);
}

#[test]
fn errors_on_unterminated_backtick_identifier() {
    assert!(lex("`abc").is_err());
}

#[test]
fn identifiers_do_not_swallow_digits_from_numbers() {
    let toks = lex("t1 1").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![TokenKind::Ident("t1".into()), TokenKind::Int(1)]
    );
}

#[test]
fn tokenizes_arithmetic_operators() {
    let toks = lex("+ - * /").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![
            TokenKind::Punct(Punct::Plus),
            TokenKind::Punct(Punct::Minus),
            TokenKind::Punct(Punct::Star),
            TokenKind::Punct(Punct::Slash),
        ]
    );
}

#[test]
fn tokenizes_percent() {
    let toks = lex("a % b").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![
            TokenKind::Ident("a".into()),
            TokenKind::Punct(Punct::Percent),
            TokenKind::Ident("b".into()),
        ]
    );
}

#[test]
fn tokenizes_comparison_operators() {
    let toks = lex("= <> != < <= > >=").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
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
fn lt_and_gt_need_whitespace() {
    let toks = lex("< >").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![TokenKind::Punct(Punct::Lt), TokenKind::Punct(Punct::Gt)]
    );
}

#[test]
fn errors_on_lone_exclamation() {
    assert!(lex("!").is_err());
}

#[test]
fn skips_line_comments() {
    let toks = lex("1 -- trailing comment\n2 -- to eof").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![TokenKind::Int(1), TokenKind::Int(2)]
    );
}

#[test]
fn comment_to_eof_without_newline() {
    let toks = lex("1 -- unterminated").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![TokenKind::Int(1)]
    );
}

#[test]
fn single_minus_is_still_minus() {
    let toks = lex("1-2").unwrap();
    assert_eq!(
        toks.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
        vec![
            TokenKind::Int(1),
            TokenKind::Punct(Punct::Minus),
            TokenKind::Int(2)
        ]
    );
}

#[test]
fn errors_on_integer_overflow() {
    assert!(lex("99999999999999999999999").is_err());
}

#[test]
fn empty_input_yields_no_tokens() {
    assert_eq!(lex("").unwrap().len(), 0);
    assert_eq!(lex(" \t\r\n").unwrap().len(), 0);
}
