use crate::value::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum ResultSet {
    Message(String),
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
}
