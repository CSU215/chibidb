use std::fmt;

/// A column's declared type. `Char(n)` bounds the character count; `Text` is
/// unbounded (subject to the page/lob limits enforced at write time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Int,
    Float,
    Char(u32),
    Date,
    Text,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Int => f.write_str("int"),
            DataType::Float => f.write_str("float"),
            DataType::Char(n) => write!(f, "char({n})"),
            DataType::Date => f.write_str("date"),
            DataType::Text => f.write_str("text"),
        }
    }
}

/// A single SQL scalar. `Bool` only ever appears as an intermediate expression
/// result; stored columns use the five declared types above.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// Days since the Unix epoch, per [`crate::datetime`].
    Date(i32),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Date(d) => f.write_str(&crate::datetime::format_date(*d)),
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(x) => {
                let s = x.to_string();
                if s.contains('.') {
                    f.write_str(&s)
                } else {
                    write!(f, "{x:.1}")
                }
            }
            Value::Str(s) => f.write_str(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_type_displays_like_sql() {
        assert_eq!(DataType::Int.to_string(), "int");
        assert_eq!(DataType::Char(10).to_string(), "char(10)");
        assert_eq!(DataType::Text.to_string(), "text");
    }

    #[test]
    fn float_always_shows_a_decimal_point() {
        assert_eq!(Value::Float(1.0).to_string(), "1.0");
        assert_eq!(Value::Float(1.5).to_string(), "1.5");
    }
}
