#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("syntax error: {0}")]
    Syntax(String),
    #[error("unsupported sql")]
    Unsupported,
}

pub type Result<T> = std::result::Result<T, Error>;
