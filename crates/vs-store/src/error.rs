//! Crate-wide error type for `vs-store`.

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("crypto: {0}")]
    Crypto(&'static str),

    #[error("keyring: {0}")]
    Keyring(String),

    #[error("not found: {kind} {id}")]
    NotFound { kind: &'static str, id: String },

    #[error("conflict: {0}")]
    Conflict(&'static str),

    #[error("invalid input: {0}")]
    Invalid(&'static str),

    #[error("master key file is the wrong size: expected 32 bytes, got {0}")]
    KeyFileSize(usize),
}

pub type Result<T> = std::result::Result<T, StoreError>;
