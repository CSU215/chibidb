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
    /// Byte offset of the token's first character in the source.
    pub pos: usize,
}

/// Splits `src` into tokens, skipping whitespace and `--` line comments.
/// Positions are byte offsets, which is what a frontend needs to underline the
/// offending text.
pub fn lex(src: &str) -> Result<Vec<Token>> {
    Lexer::new(src).run()
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    out: Vec<Token>,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, bytes: src.as_bytes(), pos: 0, out: Vec::new() }
    }

    fn run(mut self) -> Result<Vec<Token>> {
        while let Some(&c) = self.bytes.get(self.pos) {
            match c {
                b' ' | b'\t' | b'\r' | b'\n' => self.pos += 1,
                b'\'' => self.string()?,
                b'`' => self.quoted_ident()?,
                b'0'..=b'9' => self.number()?,
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.ident(),
                _ => self.punct(c)?,
            }
        }
        Ok(self.out)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn push(&mut self, kind: TokenKind, pos: usize) {
        self.out.push(Token { kind, pos });
    }

    fn number(&mut self) -> Result<()> {
        let start = self.pos;
        self.eat_digits();
        let mut is_float = false;
        // A `.` only belongs to the number when a digit follows, so `1.` stays
        // an integer followed by a dot (qualified-name punctuation).
        if self.peek() == Some(b'.') && self.bytes.get(self.pos + 1).is_some_and(u8::is_ascii_digit)
        {
            is_float = true;
            self.pos += 1;
            self.eat_digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let mut j = self.pos + 1;
            if matches!(self.bytes.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            if self.bytes.get(j).is_some_and(u8::is_ascii_digit) {
                is_float = true;
                self.pos = j;
                self.eat_digits();
            }
        }
        let text = &self.src[start..self.pos];
        let kind = if is_float {
            TokenKind::Float(text.parse().expect("lexer matched a valid float"))
        } else {
            TokenKind::Int(
                text.parse().map_err(|_| Error::syntax("integer overflow", start))?,
            )
        };
        self.push(kind, start);
        Ok(())
    }

    fn eat_digits(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
    }

    fn string(&mut self) -> Result<()> {
        let start = self.pos;
        self.pos += 1; // opening quote
        let mut s = String::new();
        loop {
            match self.peek() {
                None => return Err(Error::syntax("unterminated string", start)),
                Some(b'\'') => {
                    // `''` is an escaped quote; a lone quote ends the string.
                    if self.bytes.get(self.pos + 1) == Some(&b'\'') {
                        s.push('\'');
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                        break;
                    }
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let Some(ch) = self.src[self.pos..].chars().next() else {
                        return Err(Error::syntax("unterminated string", start));
                    };
                    s.push(unescape(ch));
                    self.pos += ch.len_utf8();
                }
                Some(_) => {
                    let ch = self.src[self.pos..].chars().next().expect("checked non-empty");
                    s.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        self.push(TokenKind::Str(s), start);
        Ok(())
    }

    fn quoted_ident(&mut self) -> Result<()> {
        let start = self.pos;
        self.pos += 1; // opening backtick
        let mut name = String::new();
        loop {
            match self.peek() {
                None => return Err(Error::syntax("unterminated identifier", start)),
                Some(b'`') => {
                    if self.bytes.get(self.pos + 1) == Some(&b'`') {
                        name.push('`');
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                        break;
                    }
                }
                Some(_) => {
                    let ch = self.src[self.pos..].chars().next().expect("checked non-empty");
                    name.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        self.push(TokenKind::Ident(name), start);
        Ok(())
    }

    fn ident(&mut self) {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            self.pos += 1;
        }
        self.push(TokenKind::Ident(self.src[start..self.pos].to_string()), start);
    }

    fn punct(&mut self, c: u8) -> Result<()> {
        let start = self.pos;
        let (p, len) = match c {
            b'(' => (Punct::LParen, 1),
            b')' => (Punct::RParen, 1),
            b',' => (Punct::Comma, 1),
            b';' => (Punct::Semicolon, 1),
            b'+' => (Punct::Plus, 1),
            b'*' => (Punct::Star, 1),
            b'/' => (Punct::Slash, 1),
            b'%' => (Punct::Percent, 1),
            b'=' => (Punct::Eq, 1),
            b'.' => (Punct::Dot, 1),
            b'-' => {
                if self.bytes.get(self.pos + 1) == Some(&b'-') {
                    self.pos += 2;
                    while self.peek().is_some_and(|b| b != b'\n') {
                        self.pos += 1;
                    }
                    return Ok(());
                }
                (Punct::Minus, 1)
            }
            b'<' => match self.bytes.get(self.pos + 1) {
                Some(b'=') => (Punct::Le, 2),
                Some(b'>') => (Punct::NotEq, 2),
                _ => (Punct::Lt, 1),
            },
            b'>' => {
                if self.bytes.get(self.pos + 1) == Some(&b'=') {
                    (Punct::Ge, 2)
                } else {
                    (Punct::Gt, 1)
                }
            }
            b'!' => {
                if self.bytes.get(self.pos + 1) == Some(&b'=') {
                    (Punct::NotEq, 2)
                } else {
                    return Err(Error::syntax("unexpected character '!'", start));
                }
            }
            _ => {
                let ch = self.src[self.pos..].chars().next();
                return Err(Error::syntax(format!("unexpected character {ch:?}"), start));
            }
        };
        self.pos += len;
        self.push(TokenKind::Punct(p), start);
        Ok(())
    }
}

/// MySQL-style backslash escapes inside a string literal.
fn unescape(ch: char) -> char {
    match ch {
        '0' => '\0',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'b' => '\u{8}',
        'Z' => '\u{1a}',
        other => other,
    }
}
