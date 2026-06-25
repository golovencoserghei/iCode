//! Framework-free error type. Concrete crates (store/embed/parse) map their
//! backend errors into these variants via `.to_string()` so `icode-core` stays
//! dependency-light and acyclic.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("store: {0}")]
    Store(String),
    #[error("embed backend: {0}")]
    Embed(String),
    #[error("vector index: {0}")]
    Vector(String),
    #[error("embedding dim mismatch: index expects {index}, got {got}")]
    DimMismatch { index: usize, got: usize },
    #[error("parse: {0}")]
    Parse(String),
    #[error("io: {0}")]
    Io(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("config: {0}")]
    Config(String),
    #[error("{0}")]
    Other(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Invalid(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
