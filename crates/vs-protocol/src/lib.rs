//! Wire-protocol encoder/decoder for vibesurfer.
//!
//! `vs-protocol` is the shared specification of the line-oriented wire
//! format documented in `docs/PROTOCOL.md` at the workspace root. The
//! daemon and the CLI both link this crate; the daemon emits, the CLI
//! parses (and vice-versa for requests).
//!
//! # Layout
//!
//! - [`codes`] — short codes ([`Role`], [`Op`], [`ErrorCode`],
//!   [`WarningCode`]) with `Display` and `FromStr` implementations.
//! - [`tree`] — in-memory [`Tree`] / [`Node`] / [`Ref`] and the
//!   indented full-tree wire format.
//! - [`delta`] — [`DeltaOp`] with encode/parse/[`apply`]/[`diff`].
//! - [`envelope`] — [`StateToken`], [`Warning`], [`Envelope`], and
//!   [`ResponseHead`].
//! - [`request`] — [`Request`] line.
//! - [`error::ParseError`] — every fallible parser returns this.

#![forbid(unsafe_code)]

pub mod codes;
pub mod delta;
pub mod envelope;
pub mod error;
pub mod request;
mod tokenize;
pub mod tree;

pub use codes::{ErrorCode, Op, Role, UnknownCode, WarningCode};
pub use delta::{apply, diff, DeltaOp};
pub use envelope::{Envelope, ResponseHead, StateToken, Warning};
pub use error::{ParseError, Result};
pub use request::Request;
pub use tree::{Node, Ref, Tree};

/// Returns the crate version (matches the workspace version).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn version_matches_cargo_pkg_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}
