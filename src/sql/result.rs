use crate::value::Value;

/// The outcome of one statement.
#[derive(Debug, Clone, PartialEq)]
pub enum ResultSet {
    Message(String),
    /// A statement that changed `n` rows (INSERT/UPDATE/DELETE).
    Affected(u64),
    Rows { columns: Vec<String>, rows: Vec<Vec<Value>> },
}

impl ResultSet {
    pub fn rows(columns: Vec<String>, rows: Vec<Vec<Value>>) -> Self {
        ResultSet::Rows { columns, rows }
    }

    pub fn message(msg: impl Into<String>) -> Self {
        ResultSet::Message(msg.into())
    }

    /// The rows of a `Rows` result, or an empty slice otherwise.
    pub fn row_data(&self) -> &[Vec<Value>] {
        match self {
            ResultSet::Rows { rows, .. } => rows,
            _ => &[],
        }
    }
}

/// Serializes statement results as the JSON envelope shared by the HTTP/JSON
/// frontend and the C ABI.
pub fn encode_results(results: &[ResultSet]) -> String {
    let mut out = String::from("{\"results\":[");
    for (i, rs) in results.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match rs {
            ResultSet::Message(m) => {
                out.push_str("{\"type\":\"message\",\"message\":");
                out.push_str(&json_string(m));
                out.push('}');
            }
            ResultSet::Affected(n) => {
                out.push_str("{\"type\":\"affected\",\"affected\":");
                out.push_str(&n.to_string());
                out.push('}');
            }
            ResultSet::Rows { columns, rows } => {
                out.push_str("{\"type\":\"rows\",\"columns\":[");
                for (j, c) in columns.iter().enumerate() {
                    if j > 0 {
                        out.push(',');
                    }
                    out.push_str(&json_string(c));
                }
                out.push_str("],\"rows\":[");
                for (j, row) in rows.iter().enumerate() {
                    if j > 0 {
                        out.push(',');
                    }
                    out.push('[');
                    for (k, v) in row.iter().enumerate() {
                        if k > 0 {
                            out.push(',');
                        }
                        out.push_str(&json_value(v));
                    }
                    out.push(']');
                }
                out.push_str("]}");
            }
        }
    }
    out.push_str("]}");
    out
}

/// Serializes an error message in the same JSON envelope.
pub fn encode_error(message: &str) -> String {
    format!("{{\"error\":{}}}", json_string(message))
}

fn json_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(x) if x.is_finite() => x.to_string(),
        Value::Float(_) => "null".to_string(),
        Value::Str(s) => json_string(s),
        Value::Date(d) => json_string(&crate::datetime::format_date(*d)),
    }
}

/// Escapes a string as a JSON literal, quotes included. Shared so the HTTP
/// admin surface does not grow a second, subtly different escaper.
pub(crate) fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_constructor_round_trips() {
        let rs = ResultSet::rows(vec!["a".into()], vec![vec![Value::Int(1)]]);
        assert_eq!(rs.row_data(), &[vec![Value::Int(1)]]);
        assert!(ResultSet::Message("ok".into()).row_data().is_empty());
    }
}
