#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("syntax error: {message}")]
    Syntax {
        message: String,
        /// Byte offset into the SQL text of the token the complaint is about,
        /// so a frontend can point at it instead of only quoting it.
        ///
        /// `None` means no particular position was implied, which is why this
        /// is not a bare `usize`: the field has to be able to say "not here",
        /// rather than nominate a wrong byte.
        pos: Option<usize>,
    },
    #[error("{0}")]
    Runtime(String),
    #[error("page full")]
    PageFull,
    /// Internal control flow: an EPQ statement restart is required under read
    /// committed. Never surfaced to a caller.
    #[doc(hidden)]
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
