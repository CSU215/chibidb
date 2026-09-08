#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported sql")]
    Unsupported,
}

pub type Result<T> = std::result::Result<T, Error>;
