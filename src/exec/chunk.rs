//! Columnar batch ("chunk") for the vectorized execution path.
//!
//! A [`Chunk`] carries up to [`CHUNK_ROWS`] rows in column-major form so scan,
//! filter, project and aggregate can operate on whole columns instead of
//! materializing one `Vec<Value>` per row. Semantics are unchanged: a chunk is
//! a transient execution container, never a storage format.

use crate::ast::DataType;
use crate::catalog::Schema;
use crate::value::Value;
use crate::{Error, Result};

/// Maximum rows carried by one chunk. Compile-time, like `PAGE_SIZE`.
pub const CHUNK_ROWS: usize = 1024;

/// One column of a chunk. `None` encodes SQL NULL.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    Bool(Vec<Option<bool>>),
    Int(Vec<Option<i64>>),
    Float(Vec<Option<f64>>),
    Str(Vec<Option<String>>),
    Date(Vec<Option<i32>>),
}

impl Column {
    /// Empty column whose element type matches a declared `dtype`.
    /// `char`/`text` both map to [`Column::Str`].
    pub fn for_dtype(dtype: DataType) -> Self {
        match dtype {
            DataType::Int => Column::Int(Vec::new()),
            DataType::Float => Column::Float(Vec::new()),
            DataType::Date => Column::Date(Vec::new()),
            DataType::Char(_) | DataType::Text => Column::Str(Vec::new()),
        }
    }

    /// Empty column shaped like `value`; a bare NULL yields a `Str` column,
    /// which still reports `Value::Null` for every element.
    fn for_value(value: &Value) -> Self {
        match value {
            Value::Bool(_) => Column::Bool(Vec::new()),
            Value::Int(_) => Column::Int(Vec::new()),
            Value::Float(_) => Column::Float(Vec::new()),
            Value::Date(_) => Column::Date(Vec::new()),
            Value::Str(_) | Value::Null => Column::Str(Vec::new()),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Column::Bool(v) => v.len(),
            Column::Int(v) => v.len(),
            Column::Float(v) => v.len(),
            Column::Str(v) => v.len(),
            Column::Date(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends one value, enforcing a single element type per column.
    pub fn push(&mut self, value: &Value) -> Result<()> {
        match self {
            Column::Bool(v) => match value {
                Value::Bool(b) => v.push(Some(*b)),
                Value::Null => v.push(None),
                _ => return Err(mismatch("bool", value)),
            },
            Column::Int(v) => match value {
                Value::Int(n) => v.push(Some(*n)),
                Value::Null => v.push(None),
                _ => return Err(mismatch("int", value)),
            },
            Column::Float(v) => match value {
                Value::Float(x) => v.push(Some(*x)),
                Value::Null => v.push(None),
                _ => return Err(mismatch("float", value)),
            },
            Column::Str(v) => match value {
                Value::Str(s) => v.push(Some(s.clone())),
                Value::Null => v.push(None),
                _ => return Err(mismatch("string", value)),
            },
            Column::Date(v) => match value {
                Value::Date(d) => v.push(Some(*d)),
                Value::Null => v.push(None),
                _ => return Err(mismatch("date", value)),
            },
        }
        Ok(())
    }

    /// Materializes element `i` back as a [`Value`].
    pub fn value(&self, i: usize) -> Value {
        match self {
            Column::Bool(v) => v[i].map(Value::Bool).unwrap_or(Value::Null),
            Column::Int(v) => v[i].map(Value::Int).unwrap_or(Value::Null),
            Column::Float(v) => v[i].map(Value::Float).unwrap_or(Value::Null),
            Column::Str(v) => v[i].as_ref().map(|s| Value::Str(s.clone())).unwrap_or(Value::Null),
            Column::Date(v) => v[i].map(Value::Date).unwrap_or(Value::Null),
        }
    }

    pub fn is_null(&self, i: usize) -> bool {
        match self {
            Column::Bool(v) => v[i].is_none(),
            Column::Int(v) => v[i].is_none(),
            Column::Float(v) => v[i].is_none(),
            Column::Str(v) => v[i].is_none(),
            Column::Date(v) => v[i].is_none(),
        }
    }

    /// Gathers the elements listed in `rows` into a new column of the same type.
    pub fn take(&self, rows: &[usize]) -> Column {
        match self {
            Column::Bool(v) => Column::Bool(rows.iter().map(|&i| v[i]).collect()),
            Column::Int(v) => Column::Int(rows.iter().map(|&i| v[i]).collect()),
            Column::Float(v) => Column::Float(rows.iter().map(|&i| v[i]).collect()),
            Column::Str(v) => Column::Str(rows.iter().map(|&i| v[i].clone()).collect()),
            Column::Date(v) => Column::Date(rows.iter().map(|&i| v[i]).collect()),
        }
    }

    /// Builds a column from materialized values, inferring the element type
    /// from the first non-NULL value (all-NULL yields a `Str` column).
    pub fn from_values(values: &[Value]) -> Result<Column> {
        let sample = values.iter().find(|v| !matches!(v, Value::Null));
        let mut column = match sample {
            Some(value) => Column::for_value(value),
            None => Column::Str(Vec::new()),
        };
        for value in values {
            column.push(value)?;
        }
        Ok(column)
    }
}

fn mismatch(want: &str, value: &Value) -> Error {
    Error::Runtime(format!("chunk column expects {want}, got {value:?}"))
}

/// A columnar batch of rows, all columns the same length.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Chunk {
    columns: Vec<Column>,
    len: usize,
}

impl Chunk {
    pub fn new() -> Self {
        Chunk { columns: Vec::new(), len: 0 }
    }

    /// Empty chunk whose columns are typed by `schema`.
    pub fn with_schema(schema: &Schema) -> Self {
        Chunk {
            columns: schema.columns.iter().map(|c| Column::for_dtype(c.dtype)).collect(),
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn num_columns(&self) -> usize {
        self.columns.len()
    }

    pub fn column(&self, index: usize) -> &Column {
        &self.columns[index]
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Appends one row to a chunk already shaped by [`Chunk::with_schema`].
    pub fn push_row(&mut self, row: &[Value]) -> Result<()> {
        if row.len() != self.columns.len() {
            return Err(Error::Runtime(format!(
                "chunk row has {} values for {} columns",
                row.len(),
                self.columns.len()
            )));
        }
        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value)?;
        }
        self.len += 1;
        Ok(())
    }

    pub fn row(&self, index: usize) -> Vec<Value> {
        self.columns.iter().map(|c| c.value(index)).collect()
    }

    /// Appends row `index` into `out`, reusing its allocation.
    pub fn row_into(&self, index: usize, out: &mut Vec<Value>) {
        out.clear();
        for column in &self.columns {
            out.push(column.value(index));
        }
    }

    /// Wraps already-built columns; all columns must be the same length.
    pub fn from_columns(columns: Vec<Column>) -> Self {
        let len = columns.first().map(Column::len).unwrap_or(0);
        Chunk { columns, len }
    }

    pub fn from_row(row: &[Value]) -> Result<Self> {
        Self::from_rows(&[row.to_vec()])
    }

    /// Builds a chunk from equal-width rows, inferring each column's type from
    /// its first non-NULL element (all-NULL columns become [`Column::Str`]).
    pub fn from_rows(rows: &[Vec<Value>]) -> Result<Self> {
        if rows.is_empty() {
            return Ok(Chunk::new());
        }
        let width = rows[0].len();
        let mut columns = Vec::with_capacity(width);
        for i in 0..width {
            let sample = rows.iter().map(|r| &r[i]).find(|v| !matches!(v, Value::Null));
            let mut column = match sample {
                Some(value) => Column::for_value(value),
                None => Column::Str(Vec::new()),
            };
            for row in rows {
                column.push(&row[i])?;
            }
            columns.push(column);
        }
        Ok(Chunk { columns, len: rows.len() })
    }

    pub fn to_rows(&self) -> Vec<Vec<Value>> {
        (0..self.len).map(|i| self.row(i)).collect()
    }

    /// Keeps only the rows listed in `rows` (indices in any order).
    pub fn take(&self, rows: &[usize]) -> Chunk {
        Chunk {
            columns: self.columns.iter().map(|c| c.take(rows)).collect(),
            len: rows.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDesc;

    #[test]
    fn from_rows_infers_types_and_preserves_nulls() {
        let rows = vec![
            vec![Value::Int(1), Value::Str("a".into())],
            vec![Value::Null, Value::Null],
            vec![Value::Int(3), Value::Str("c".into())],
        ];
        let chunk = Chunk::from_rows(&rows).unwrap();
        assert_eq!(chunk.len(), 3);
        assert_eq!(chunk.column(0), &Column::Int(vec![Some(1), None, Some(3)]));
        assert_eq!(
            chunk.column(1),
            &Column::Str(vec![Some("a".into()), None, Some("c".into())])
        );
        assert_eq!(chunk.to_rows(), rows);
    }

    #[test]
    fn all_null_column_is_str_and_reports_null() {
        let rows = vec![vec![Value::Null], vec![Value::Null]];
        let chunk = Chunk::from_rows(&rows).unwrap();
        assert_eq!(chunk.column(0), &Column::Str(vec![None, None]));
        assert_eq!(chunk.row(1), vec![Value::Null]);
    }

    #[test]
    fn with_schema_shapes_typed_columns() {
        let schema = Schema {
            columns: vec![
                ColumnDesc::plain(None, "a".into(), DataType::Int),
                ColumnDesc::plain(None, "b".into(), DataType::Date),
            ],
        };
        let mut chunk = Chunk::with_schema(&schema);
        chunk.push_row(&[Value::Int(7), Value::Date(0)]).unwrap();
        assert_eq!(chunk.column(0), &Column::Int(vec![Some(7)]));
        assert_eq!(chunk.column(1), &Column::Date(vec![Some(0)]));
    }

    #[test]
    fn take_gathers_rows_in_order() {
        let rows =
            vec![vec![Value::Int(10)], vec![Value::Int(20)], vec![Value::Int(30)]];
        let chunk = Chunk::from_rows(&rows).unwrap();
        let picked = chunk.take(&[2, 0]);
        assert_eq!(picked.to_rows(), vec![vec![Value::Int(30)], vec![Value::Int(10)]]);
    }

    #[test]
    fn push_rejects_wrong_type() {
        let mut column = Column::Int(Vec::new());
        assert!(column.push(&Value::Str("x".into())).is_err());
    }
}
