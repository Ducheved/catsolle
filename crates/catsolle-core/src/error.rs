use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("database error: {0}")]
    Database(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ssh error: {0}")]
    Ssh(String),
    #[error("invalid data: {0}")]
    Invalid(String),
    #[error("not found")]
    NotFound,
}
