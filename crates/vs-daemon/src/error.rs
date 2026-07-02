//! Crate-wide error type for the daemon.
//!
//! [`DaemonError`] aggregates the typed errors from the layers below
//! (`vs-protocol`, `vs-store`, `vs-engine-webkit`) plus daemon-specific
//! cases (no active session, unknown page, stale token).
//!
//! Every error has a wire mapping in [`DaemonError::wire`] — the form
//! the daemon emits as `! <CODE> [args]` per `docs/PROTOCOL.md`.

use vs_protocol::{ErrorCode, ParseError, StateToken};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DaemonError {
    #[error("protocol parse: {0}")]
    Protocol(#[from] ParseError),

    #[error("store: {0}")]
    Store(#[from] vs_store::StoreError),

    #[error("engine: {0}")]
    Engine(#[from] vs_engine_webkit::EngineError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("anyhow: {0}")]
    Other(#[from] anyhow::Error),

    /// The agent's pre-image token did not match the page's current
    /// token.
    #[error("stale token: current={current}, reason={reason}")]
    StaleToken {
        current: StateToken,
        reason: &'static str,
    },

    /// No active session for this connection.
    #[error("no active session")]
    NoSession,

    /// Session id refers to no known session.
    #[error("unknown session: {0}")]
    UnknownSession(String),

    /// Page id refers to no known page in this session.
    #[error("unknown page: {0}")]
    UnknownPage(String),

    /// The page exists, but in a different session than the one the
    /// caller addressed (page ids are globally unique). Distinguishes
    /// "wrong session" from "no such page" so the caller can switch
    /// instead of assuming the page was lost.
    #[error("page {page} is in session {page_session}, not addressed session {addressed}")]
    WrongSession {
        page: String,
        addressed: String,
        page_session: String,
    },

    /// Ref does not exist in the page's current tree.
    #[error("unknown ref: {0}")]
    UnknownRef(u32),

    /// The wire request was malformed.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Engine's capability set excludes this primitive on this platform.
    #[error("unsupported on this engine: {primitive} ({engine})")]
    Unsupported {
        engine: &'static str,
        primitive: &'static str,
    },
}

impl DaemonError {
    /// The wire-format error code and args for this error.
    #[must_use]
    pub fn wire(&self) -> (ErrorCode, Vec<String>) {
        match self {
            Self::StaleToken { current, reason } => (
                ErrorCode::StaleToken,
                vec![current.to_string(), (*reason).to_string()],
            ),
            Self::NoSession => (ErrorCode::BadRequest, vec!["no_session".into()]),
            Self::UnknownSession(id) => (ErrorCode::NotFound, vec![format!("session={id}")]),
            Self::UnknownPage(id) => (ErrorCode::NotFound, vec![format!("page={id}")]),
            Self::WrongSession {
                page,
                addressed,
                page_session,
            } => (
                ErrorCode::WrongSession,
                vec![
                    format!("page={page}"),
                    format!("addressed={addressed}"),
                    format!("page_session={page_session}"),
                ],
            ),
            Self::UnknownRef(r) => (ErrorCode::NotFound, vec![format!("ref={r}")]),
            Self::BadRequest(msg) => (ErrorCode::BadRequest, vec![msg.clone()]),
            Self::Unsupported { engine, primitive }
            | Self::Engine(vs_engine_webkit::EngineError::Unsupported { engine, primitive }) => (
                ErrorCode::EngineUnsupported,
                vec![(*primitive).to_string(), (*engine).to_string()],
            ),
            Self::Engine(vs_engine_webkit::EngineError::Timeout { primitive, budget }) => (
                ErrorCode::Timeout,
                vec![
                    format!("{}ms", budget.as_millis()),
                    (*primitive).to_string(),
                ],
            ),
            Self::Engine(vs_engine_webkit::EngineError::NotFound { kind, id }) => {
                (ErrorCode::NotFound, vec![format!("{kind}={id}")])
            }
            Self::Engine(vs_engine_webkit::EngineError::Crashed) => {
                (ErrorCode::EngineCrash, vec![])
            }
            Self::Engine(_) => (ErrorCode::BadRequest, vec![format!("engine: {self}")]),
            Self::Protocol(_) => (ErrorCode::BadRequest, vec![format!("{self}")]),
            Self::Store(_) | Self::Io(_) | Self::Other(_) => {
                (ErrorCode::BadRequest, vec![format!("{self}")])
            }
        }
    }
}

pub type Result<T> = std::result::Result<T, DaemonError>;
