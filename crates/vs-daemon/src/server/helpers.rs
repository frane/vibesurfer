//! Shared helpers used by every wire handler: session lookup,
//! flag-value extraction, error formatting.

use vs_protocol::{Envelope, ErrorCode, Request};

use crate::error::DaemonError;

pub(super) fn require_session(req: &Request) -> std::result::Result<String, String> {
    flag_value(req, "session").ok_or_else(|| "missing --session=<id>".to_string())
}

pub(super) fn flag_value(req: &Request, name: &str) -> Option<String> {
    req.flags.get(name).and_then(Clone::clone)
}

pub(super) fn format_error(code: ErrorCode, args: Vec<String>) -> String {
    Envelope::Error { code, args }.encode()
}

pub(super) fn format_daemon_error(e: &DaemonError) -> String {
    let (code, args) = e.wire();
    format_error(code, args)
}
