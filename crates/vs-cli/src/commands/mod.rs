//! `clap`-derived command tree and dispatch.
//!
//! Each primitive is one subcommand. [`run`] builds a [`Request`] from
//! the subcommand, calls [`Client::call`](crate::client::Client::call),
//! and returns the response; [`render`] formats it for stdout.

mod cli;
mod dispatch;
mod render;
mod request;

pub use cli::{CaptureSub, Cli, Command, FlowSub, NetOverrides, NetSub, PendingSub, RecordSub};
pub use dispatch::{connect, resolve_paths, resolve_session, run};
pub use render::render;
