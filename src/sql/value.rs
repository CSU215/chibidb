use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
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
