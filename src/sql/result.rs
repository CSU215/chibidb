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
