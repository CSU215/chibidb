use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Punct {
    LParen,
    RParen,
    Comma,
    Semicolon,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Eq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    Dot,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Int(i64),
    Float(f64),
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
                let mut is_float =
                    if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
                        i += 1;
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                        true
                    } else {
                        false
                    };
                if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
                    let mut j = i + 1;
                    if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j].is_ascii_digit() {
                        is_float = true;
                        i = j;
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                }
                let kind = if is_float {
                    TokenKind::Float(src[start..i].parse().unwrap())
                } else {
                    let n: i64 = src[start..i].parse().map_err(|_| {
                        Error::Syntax(format!("integer overflow at byte {start}"))
                    })?;
                    TokenKind::Int(n)
                };
                out.push(Token { kind, pos: start });
            }
            b'\'' => {
                let start = i;
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= bytes.len() {
                        return Err(Error::Syntax(format!(
                            "unterminated string at byte {start}"
                        )));
                    }
                    match bytes[i] {
                        b'\'' => {
                            // `''` is an escaped quote; a lone quote ends it.
                            if bytes.get(i + 1) == Some(&b'\'') {
                                s.push('\'');
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        }
                        b'\\' => {
                            // MySQL backslash escapes (the default mode).
                            i += 1;
                            let ch = src[i..].chars().next().ok_or_else(|| {
                                Error::Syntax(format!("unterminated string at byte {start}"))
                            })?;
                            s.push(match ch {
                                '0' => '\0',
                                'n' => '\n',
                                'r' => '\r',
                                't' => '\t',
                                'b' => '\u{8}',
                                'Z' => '\u{1a}',
                                other => other,
                            });
                            i += ch.len_utf8();
                        }
                        _ => {
                            let ch = src[i..].chars().next().unwrap();
                            s.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                }
                out.push(Token { kind: TokenKind::Str(s), pos: start });
            }
            b'`' => {
                // MySQL-style quoted identifier: `name`, with `` as an
                // escaped backtick. Decoded to a plain identifier token.
                let start = i;
                i += 1;
                let mut name = String::new();
                loop {
                    if i >= bytes.len() {
                        return Err(Error::Syntax(format!(
                            "unterminated identifier at byte {start}"
                        )));
                    }
                    if bytes[i] == b'`' {
                        if bytes.get(i + 1) == Some(&b'`') {
                            name.push('`');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        let ch = src[i..].chars().next().unwrap();
                        name.push(ch);
                        i += ch.len_utf8();
                    }
                }
                out.push(Token { kind: TokenKind::Ident(name), pos: start });
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                out.push(Token { kind: TokenKind::Ident(src[start..i].to_string()), pos: start });
            }
            b'(' => push_punct(&mut out, &mut i, Punct::LParen, 1),
            b')' => push_punct(&mut out, &mut i, Punct::RParen, 1),
            b',' => push_punct(&mut out, &mut i, Punct::Comma, 1),
            b';' => push_punct(&mut out, &mut i, Punct::Semicolon, 1),
            b'+' => push_punct(&mut out, &mut i, Punct::Plus, 1),
            b'-' => {
                if bytes.get(i + 1) == Some(&b'-') {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                } else {
                    push_punct(&mut out, &mut i, Punct::Minus, 1);
                }
            }
            b'*' => push_punct(&mut out, &mut i, Punct::Star, 1),
            b'/' => push_punct(&mut out, &mut i, Punct::Slash, 1),
            b'%' => push_punct(&mut out, &mut i, Punct::Percent, 1),
            b'=' => push_punct(&mut out, &mut i, Punct::Eq, 1),
            b'.' => push_punct(&mut out, &mut i, Punct::Dot, 1),
            b'<' => {
                let (p, len) = match bytes.get(i + 1) {
                    Some(b'=') => (Punct::Le, 2),
                    Some(b'>') => (Punct::NotEq, 2),
                    _ => (Punct::Lt, 1),
                };
                push_punct(&mut out, &mut i, p, len);
            }
            b'>' => {
                let (p, len) =
                    if bytes.get(i + 1) == Some(&b'=') { (Punct::Ge, 2) } else { (Punct::Gt, 1) };
                push_punct(&mut out, &mut i, p, len);
            }
            b'!' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    push_punct(&mut out, &mut i, Punct::NotEq, 2);
                } else {
                    return Err(Error::Syntax(format!(
                        "unexpected character '!' at byte {i}"
                    )));
                }
            }
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

fn push_punct(out: &mut Vec<Token>, i: &mut usize, p: Punct, len: usize) {
    out.push(Token { kind: TokenKind::Punct(p), pos: *i });
    *i += len;
}
