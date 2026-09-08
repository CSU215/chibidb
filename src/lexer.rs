use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Punct {
    LParen,
    RParen,
    Comma,
    Semicolon,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Int(i64),
    Str(String),
    Ident(String),
    Punct(Punct),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub pos: usize,
}

pub fn lex(src: &str) -> Result<Vec<Token>> {
    let bytes = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'0'..=b'9' => {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let n: i64 = src[start..i]
                    .parse()
                    .map_err(|_| Error::Syntax(format!("integer overflow at byte {start}")))?;
                out.push(Token { kind: TokenKind::Int(n), pos: start });
            }
            b'(' => push_punct(&mut out, &mut i, Punct::LParen),
            b')' => push_punct(&mut out, &mut i, Punct::RParen),
            b',' => push_punct(&mut out, &mut i, Punct::Comma),
            b';' => push_punct(&mut out, &mut i, Punct::Semicolon),
            _ => {
                let ch = src.get(i..).and_then(|s| s.chars().next());
                return Err(Error::Syntax(format!(
                    "unexpected character {ch:?} at byte {i}"
                )));
            }
        }
    }
    Ok(out)
}

fn push_punct(out: &mut Vec<Token>, i: &mut usize, p: Punct) {
    out.push(Token { kind: TokenKind::Punct(p), pos: *i });
    *i += 1;
}
