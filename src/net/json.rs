//! A minimal JSON reader, for request bodies on the HTTP surface.
//!
//! There is no `serde_json` in the dependency set and the project's rule is not
//! to add one, so this is hand-written. It replaces `json_string_field`, which
//! could only dig out one top-level string and so could not express
//! `{"sql": "...", "db": "shop"}`.

use crate::{Error, Result};

/// Maximum nesting depth.
///
/// A recursive descent over a hostile body of `[` characters would overflow the
/// thread stack, and a stack overflow **aborts** the process rather than
/// unwinding -- it is not a catchable panic. The request size limit does not
/// help here, since 8 MiB is far more `[` than a stack can hold.
const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// The value of a key in an object, or `None` for any other shape.
    pub(crate) fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(name, _)| name == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(text) => Some(text),
            _ => None,
        }
    }
}

pub(crate) fn parse(text: &str) -> Result<Json> {
    let mut parser = Parser { text, pos: 0 };
    parser.skip_ws();
    let value = parser.value(0)?;
    parser.skip_ws();
    if parser.pos != text.len() {
        return Err(parser.error("trailing characters"));
    }
    Ok(value)
}

struct Parser<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn rest(&self) -> &'a str {
        &self.text[self.pos..]
    }

    fn error(&self, what: &str) -> Error {
        Error::Runtime(format!("invalid JSON at byte {}: {what}", self.pos))
    }

    fn skip_ws(&mut self) {
        while self.rest().starts_with(|c: char| c.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    /// Consumes `c` if it is next.
    fn eat(&mut self, c: char) -> bool {
        if self.rest().starts_with(c) {
            self.pos += c.len_utf8();
            true
        } else {
            false
        }
    }

    fn take(&mut self, n: usize) -> Option<&'a str> {
        let end = self.pos + n;
        // `get` rather than slicing: a multi-byte character could straddle `end`.
        let slice = self.text.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn value(&mut self, depth: usize) -> Result<Json> {
        // Checked before recursing, which is what keeps a body of `[` from
        // reaching the stack limit.
        if depth > MAX_DEPTH {
            return Err(self.error("nesting too deep"));
        }
        match self.rest().chars().next() {
            None => Err(self.error("unexpected end of input")),
            Some('{') => self.object(depth),
            Some('[') => self.array(depth),
            Some('"') => Ok(Json::Str(self.string()?)),
            Some('t') => self.literal("true").map(|()| Json::Bool(true)),
            Some('f') => self.literal("false").map(|()| Json::Bool(false)),
            Some('n') => self.literal("null").map(|()| Json::Null),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            Some(c) => Err(self.error(&format!("unexpected character {c:?}"))),
        }
    }

    fn literal(&mut self, word: &str) -> Result<()> {
        if self.rest().starts_with(word) {
            self.pos += word.len();
            Ok(())
        } else {
            Err(self.error(&format!("expected {word}")))
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json> {
        self.eat('{');
        let mut fields = Vec::new();
        self.skip_ws();
        if self.eat('}') {
            return Ok(Json::Obj(fields));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if !self.eat(':') {
                return Err(self.error("expected ':' after an object key"));
            }
            self.skip_ws();
            fields.push((key, self.value(depth + 1)?));
            self.skip_ws();
            if self.eat(',') {
                continue;
            }
            if self.eat('}') {
                return Ok(Json::Obj(fields));
            }
            return Err(self.error("expected ',' or '}'"));
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json> {
        self.eat('[');
        let mut items = Vec::new();
        self.skip_ws();
        if self.eat(']') {
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            if self.eat(',') {
                continue;
            }
            if self.eat(']') {
                return Ok(Json::Arr(items));
            }
            return Err(self.error("expected ',' or ']'"));
        }
    }

    fn string(&mut self) -> Result<String> {
        if !self.eat('"') {
            return Err(self.error("expected a string"));
        }
        let mut out = String::new();
        loop {
            let Some(c) = self.rest().chars().next() else {
                return Err(self.error("unterminated string"));
            };
            self.pos += c.len_utf8();
            match c {
                '"' => return Ok(out),
                '\\' => out.push(self.escape()?),
                // Raw control characters are not legal inside a JSON string.
                c if (c as u32) < 0x20 => return Err(self.error("raw control character in string")),
                c => out.push(c),
            }
        }
    }

    fn escape(&mut self) -> Result<char> {
        let Some(c) = self.rest().chars().next() else {
            return Err(self.error("unterminated escape"));
        };
        self.pos += c.len_utf8();
        Ok(match c {
            '"' => '"',
            '\\' => '\\',
            '/' => '/',
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'u' => self.unicode_escape()?,
            other => return Err(self.error(&format!("unknown escape \\{other}"))),
        })
    }

    fn unicode_escape(&mut self) -> Result<char> {
        let hex = self.take(4).ok_or_else(|| self.error("truncated \\u escape"))?;
        let code = u32::from_str_radix(hex, 16).map_err(|_| self.error("bad \\u escape"))?;
        // Surrogate halves are legal JSON but decode to no character; SQL text
        // has no use for them, so rejecting is better than emitting a mystery.
        char::from_u32(code).ok_or_else(|| self.error("unpaired surrogate in \\u escape"))
    }

    fn number(&mut self) -> Result<Json> {
        let start = self.pos;
        self.eat('-');
        // Integer part: a lone `0`, or a non-zero digit followed by any digits.
        // That is what makes `01` illegal.
        if self.eat('0') {
            // done
        } else if !self.consume_digits() {
            return Err(self.error("expected a digit"));
        }
        if self.eat('.') && !self.consume_digits() {
            return Err(self.error("expected a digit after '.'"));
        }
        if matches!(self.rest().chars().next(), Some('e' | 'E')) {
            self.pos += 1;
            if matches!(self.rest().chars().next(), Some('+' | '-')) {
                self.pos += 1;
            }
            if !self.consume_digits() {
                return Err(self.error("expected a digit in the exponent"));
            }
        }
        self.text[start..self.pos]
            .parse::<f64>()
            .map(Json::Num)
            .map_err(|_| self.error("malformed number"))
    }

    fn consume_digits(&mut self) -> bool {
        let start = self.pos;
        while self.rest().starts_with(|c: char| c.is_ascii_digit()) {
            self.pos += 1;
        }
        self.pos > start
    }
}

/// Reads one top-level string field, the way the original `json_string_field`
/// did. Kept as a wrapper so callers (and its unit test in `http.rs`) do not
/// have to move.
pub(crate) fn json_string_field(text: &str, key: &str) -> Option<String> {
    parse(text).ok()?.get(key)?.as_str().map(str::to_owned)
}

// The JSON result envelope lives with `ResultSet` (as in the reference crate);
// it is re-exported here so the HTTP/admin surface has a single import site.
pub(crate) use crate::sql::result::{encode_error, encode_results, json_string};

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(json: &Json, key: &str) -> Json {
        json.get(key).cloned().unwrap_or_else(|| panic!("missing key {key}: {json:?}"))
    }

    #[test]
    fn parses_objects_arrays_and_scalars() {
        let json = parse(r#"{"a": 1, "b": "two", "c": [true, false, null], "d": {"e": 2.5}}"#)
            .unwrap();
        assert_eq!(obj(&json, "a"), Json::Num(1.0));
        assert_eq!(obj(&json, "b"), Json::Str("two".into()));
        assert_eq!(
            obj(&json, "c"),
            Json::Arr(vec![Json::Bool(true), Json::Bool(false), Json::Null])
        );
        assert_eq!(obj(&obj(&json, "d"), "e"), Json::Num(2.5));

        // Whitespace is allowed anywhere between tokens.
        assert_eq!(parse("  [ 1 , 2 ]  ").unwrap(), Json::Arr(vec![Json::Num(1.0), Json::Num(2.0)]));
        assert_eq!(parse("{}").unwrap(), Json::Obj(Vec::new()));
        assert_eq!(parse("[]").unwrap(), Json::Arr(Vec::new()));
    }

    #[test]
    fn parses_every_escape() {
        let json = parse(r#""\" \\ \/ \n \t \r \b \f A é""#).unwrap();
        assert_eq!(
            json,
            Json::Str("\" \\ / \n \t \r \u{8} \u{c} A é".into())
        );
    }

    #[test]
    fn parses_numbers() {
        assert_eq!(parse("0").unwrap(), Json::Num(0.0));
        assert_eq!(parse("-0").unwrap(), Json::Num(0.0));
        assert_eq!(parse("-12.75").unwrap(), Json::Num(-12.75));
        assert_eq!(parse("1e3").unwrap(), Json::Num(1000.0));
        assert_eq!(parse("1E+3").unwrap(), Json::Num(1000.0));
        assert_eq!(parse("2e-2").unwrap(), Json::Num(0.02));
        assert_eq!(parse("123456789").unwrap(), Json::Num(123456789.0));
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "",
            "   ",
            "{",
            "}",
            "[1",
            "[1,]",
            "{\"a\":}",
            "{\"a\" 1}",
            "{a: 1}",
            "{\"a\":1,}",
            "'single'",
            "nul",
            "tru",
            "TRUE",
            "\"unterminated",
            "\"bad \\q escape\"",
            "\"bad \\u12 escape\"",
            "01",
            "+1",
            ".5",
            "5.",
            "1.2.3",
            "1e",
            "NaN",
            "Infinity",
        ] {
            assert!(parse(bad).is_err(), "should have been rejected: {bad:?}");
        }
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(parse("1 2").is_err());
        assert!(parse("{} {}").is_err());
        assert!(parse("[1] x").is_err());
    }

    #[test]
    fn nesting_is_capped_at_the_documented_depth() {
        let depth = |levels: usize| {
            format!("{}1{}", "[".repeat(levels), "]".repeat(levels))
        };
        // Exactly at the limit is fine; one deeper is not.
        assert!(parse(&depth(MAX_DEPTH)).is_ok(), "depth {MAX_DEPTH} should parse");
        assert!(parse(&depth(MAX_DEPTH + 1)).is_err(), "depth {MAX_DEPTH} + 1 must be rejected");

        // The real hazard: a body of nothing but open brackets must be refused
        // rather than recursed into until the stack dies.
        let hostile = "[".repeat(100_000);
        assert!(parse(&hostile).is_err());
    }

    #[test]
    fn duplicate_keys_keep_the_first() {
        // `get` scans in order, so the first wins. JSON leaves duplicates
        // undefined; the point here is that it is decided, not accidental.
        let json = parse(r#"{"a": 1, "a": 2}"#).unwrap();
        assert_eq!(obj(&json, "a"), Json::Num(1.0));
    }

    #[test]
    fn json_string_field_wraps_the_parser() {
        assert_eq!(
            json_string_field(r#"{"sql": "select \"x\";\n"}"#, "sql").unwrap(),
            "select \"x\";\n"
        );
        assert!(json_string_field("{}", "sql").is_none());
        // Not a string, not this key, or not JSON at all.
        assert!(json_string_field(r#"{"sql": 1}"#, "sql").is_none());
        assert!(json_string_field(r#"{"db": "shop"}"#, "sql").is_none());
        assert!(json_string_field("not json", "sql").is_none());
    }
}
