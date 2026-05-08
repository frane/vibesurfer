//! `vs` — the vibesurfer CLI.
//!
//! The binary at `src/main.rs` is the CLI front-end; this library
//! holds the reusable pieces (client, active-session, spawn, command
//! dispatch) so integration tests can construct flows without forking
//! the binary.

#![cfg_attr(not(target_os = "macos"), forbid(unsafe_code))]
#![cfg_attr(target_os = "macos", allow(unsafe_code))]

pub mod active_session;
pub mod client;
pub mod commands;
pub mod mcp;
pub mod paths;
pub mod serve;
pub mod skill_install;
pub mod spawn;

pub use client::{Client, Response};

/// Returns the crate version (matches the workspace version).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
