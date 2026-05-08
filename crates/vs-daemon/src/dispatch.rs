//! Dispatcher: the single entry point that takes one or more
//! primitives and returns one rendered wire response per primitive.
//!
//! Both the wire server (`server/mod.rs`) and in-process tests funnel
//! through [`Daemon::dispatch`]. Today the wire delivers exactly one
//! primitive per frame, so the inbound vec has length 1 and the
//! response is a single rendered envelope+body. M5.5 PRs 2–6 introduce
//! composite-flag primitives that internally expand into multiple
//! sequenced primitives. The wire syntax for explicit pipelines (v2,
//! see ADR 0007) lands as a parser-only change against this same
//! dispatcher — no further dispatcher rewrite required.
//!
//! ## Why `Primitive = Request` (today)
//!
//! `Request` already carries everything the dispatcher needs: the
//! primitive name, positional args, and a flag map. Composite flags
//! (`--view`, `--layout=`, `--read=`) are entries in that map, not new
//! variants. A typed `Primitive` enum may land in a later milestone
//! once we have evidence that the dispatcher's match-on-name is
//! genuinely brittle in practice. Until then it would be type-system
//! ceremony for no payoff.

use vs_protocol::Request;

/// A single primitive in a dispatch sequence. Today this is a
/// re-export of [`vs_protocol::Request`]; see the module docs.
pub type Primitive = Request;

/// One rendered response, in the canonical wire shape: warnings (if
/// any) + envelope + body lines, terminated by a single `\n`. The
/// dispatcher never inserts the framing blank line — the caller
/// (`server::handle_connection`) does that between outcomes.
#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    /// Already-formatted wire bytes (warnings + envelope + body).
    pub wire: String,
}

impl DispatchOutcome {
    /// Wrap a pre-rendered wire string.
    #[must_use]
    pub fn from_wire(s: String) -> Self {
        Self { wire: s }
    }
}
