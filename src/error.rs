#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("syntax error: {0}")]
    Syntax(String),
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

pub type Result<T> = std::result::Result<T, Error>;
