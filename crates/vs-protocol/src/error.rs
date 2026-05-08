//! Crate-wide parse errors.
//!
//! Every fallible parser in this crate returns [`ParseError`]. Variants
//! carry enough context to identify what went wrong without echoing the
//! whole input — the wire is meant to be machine-readable, so the error
//! is the structured diagnostic, not the line.

use crate::codes::UnknownCode;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseError {
    #[error("empty input")]
    EmptyInput,

    #[error("indent jumped from depth {from} to depth {to} on line {line}")]
    IndentJump { from: usize, to: usize, line: usize },

    #[error("malformed line {line}: {message}")]
    MalformedLine { line: usize, message: &'static str },

    #[error("invalid ref {raw:?}")]
    InvalidRef { raw: String },

    #[error("invalid position {raw:?}")]
    InvalidPosition { raw: String },

    #[error("missing field: {field}")]
    MissingField { field: &'static str },

    #[error("unknown code: {0}")]
    UnknownCode(#[from] UnknownCode),

    #[error("invalid envelope: {detail}")]
    InvalidEnvelope { detail: &'static str },

    #[error("invalid state token {raw:?}: expected 16 lowercase hex chars")]
    InvalidToken { raw: String },

    #[error("invalid attribute {raw:?}")]
    InvalidAttribute { raw: String },

    #[error("invalid request: {detail}")]
    InvalidRequest { detail: &'static str },

    #[error("invalid label {raw:?}: contains an unterminated quoted span")]
    UnterminatedQuote { raw: String },

    #[error("delta apply: ref {0} not present in source tree")]
    ApplyMissingRef(u32),

    #[error("delta apply: ref {0} already exists in source tree")]
    ApplyDuplicateRef(u32),

    #[error("delta apply: parent ref {0} not present")]
    ApplyMissingParent(u32),
}

pub type Result<T> = std::result::Result<T, ParseError>;
