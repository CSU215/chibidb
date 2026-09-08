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
    assert!(lex("1.2").is_err(), "dot is not a token yet");
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
