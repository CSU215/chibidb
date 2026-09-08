#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("syntax error: {0}")]
    Syntax(String),
    #[error("{0}")]
    Runtime(String),
    #[error("page full")]
    PageFull,
    #[error("unsupported sql")]
    Unsupported,
}

pub type Result<T> = std::result::Result<T, Error>;
