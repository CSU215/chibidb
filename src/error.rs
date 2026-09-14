#![forbid(unsafe_code)]

/// Errors surfaced to callers. Engine-internal control flow (e.g. a statement
/// restart) lives in its own variants and never reaches a user unchanged.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("syntax error: {message}")]
    Syntax {
        message: String,
        /// Byte offset into the SQL text the complaint is about, so a frontend
        /// can point at it instead of only quoting it. `None` means no
        /// particular position was implied.
        pos: Option<usize>,
    },
    #[error("{0}")]
    Runtime(String),
    #[error("page full")]
    PageFull,
    /// Internal control flow: a read-committed statement must be restarted
    /// (EPQ). Never surfaces to a caller as-is.
    #[error("retry statement")]
    Retry,
    #[error("unsupported sql")]
    Unsupported,
}

impl Error {
    /// A syntax error at `pos` bytes into the statement.
    pub fn syntax(message: impl Into<String>, pos: usize) -> Self {
        Self::Syntax { message: message.into(), pos: Some(pos) }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_error_carries_its_byte_offset() {
        let e = Error::syntax("boom", 7);
        assert_eq!(e.to_string(), "syntax error: boom");
        match e {
            Error::Syntax { message, pos } => {
                assert_eq!(message, "boom");
                assert_eq!(pos, Some(7));
            }
            other => panic!("expected Syntax, got {other:?}"),
        }
    }
}
