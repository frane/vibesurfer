//! `vibesurferd` — the vibesurfer daemon library.
//!
//! The binary at `src/main.rs` is a thin wrapper around this library:
//! it constructs a [`Daemon`], binds a Unix socket, and runs the
//! [`server`] loop. Tests bypass the binary and drive [`Daemon`]
//! directly.

#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

pub mod config;
pub mod daemon;
pub mod dispatch;
pub mod error;
pub mod page_state;
pub mod redact;
pub mod server;
pub mod tokens;
pub mod transport;

pub use daemon::{
    ActCall, ActResponse, AnnotateResponse, AuthClearResponse, AuthListResponse, AuthLoadResponse,
    AuthSaveResponse, CaptureResponse, CloseResponse, Daemon, ExtractResponse, FindHit,
    FindResponse, LayoutResponse, LogResponse, MarkResponse, OpenResponse, ReadResponse,
    SessionCloseResponse, SessionOpenResponse, SkillListResponse, SkillShowResponse,
    StatusResponse, ViewResponse, ViewportResponse, WaitResponse,
};
pub use dispatch::{DispatchOutcome, Primitive};
pub use error::{DaemonError, Result};
pub use page_state::{PageState, ViewForm};

/// Returns the crate version (matches the workspace version).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
