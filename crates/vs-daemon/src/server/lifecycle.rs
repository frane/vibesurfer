//! Wire handlers for session/page lifecycle.

use vs_protocol::{ErrorCode, Request, ResponseHead, StateToken};

use super::helpers::{flag_value, format_daemon_error, format_error, require_session};
use crate::daemon::{
    CloseResponse, Daemon, OpenResponse, SessionCloseResponse, SessionOpenResponse,
};

pub(super) fn handle_session_open(daemon: &Daemon, req: &Request) -> String {
    let policy = flag_value(req, "policy");
    match daemon.session_open(policy.as_deref()) {
        Ok(SessionOpenResponse { session_id, token }) => {
            let head = ResponseHead::ok(token).encode();
            format!("{head}{session_id}\n")
        }
        Err(e) => format_daemon_error(&e),
    }
}

pub(super) fn handle_session_close(daemon: &Daemon, req: &Request) -> String {
    // `--all` / `--idle-for=<dur>` sweep the workspace; a bare id
    // closes one session, as it always has.
    let idle_for = match flag_value(req, "idle-for") {
        Some(raw) => match parse_duration(&raw) {
            Some(d) => Some(Some(d)),
            None => {
                return format_error(
                    ErrorCode::BadRequest,
                    vec![format!(
                        "vs_session_close: bad --idle-for {raw:?} (try 2h, 90m, 30s)"
                    )],
                )
            }
        },
        None => req.flags.contains_key("all").then_some(None),
    };
    if let Some(window) = idle_for {
        return match daemon.session_close_sweep(window) {
            Ok(closed) => {
                let mut body = String::new();
                for id in &closed {
                    body.push_str(id);
                    body.push('\n');
                }
                format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
            }
            Err(e) => format_daemon_error(&e),
        };
    }
    let Some(session_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_session_close: missing session id".into()],
        );
    };
    match daemon.session_close(&session_id) {
        Ok(SessionCloseResponse) => ResponseHead::ok(StateToken::ZERO).encode(),
        Err(e) => format_daemon_error(&e),
    }
}

/// `2h` / `90m` / `30s` / a bare number of seconds.
fn parse_duration(raw: &str) -> Option<std::time::Duration> {
    let raw = raw.trim();
    let (digits, mult) = match raw.as_bytes().last()? {
        b'h' => (&raw[..raw.len() - 1], 3600),
        b'm' => (&raw[..raw.len() - 1], 60),
        b's' => (&raw[..raw.len() - 1], 1),
        _ => (raw, 1),
    };
    let n: u64 = digits.trim().parse().ok()?;
    Some(std::time::Duration::from_secs(n * mult))
}

pub(super) fn handle_open(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(url) = req.args.first().cloned() else {
        return format_error(ErrorCode::BadRequest, vec!["vs_open: missing url".into()]);
    };
    match daemon.open(&session_id, &url) {
        Ok(OpenResponse {
            page_id,
            token,
            warnings,
        }) => {
            let mut head = ResponseHead::ok(token);
            head.warnings = warnings;
            let open_wire = format!("{}{page_id}\n", head.encode());

            // M5.5 PR2: `vs_open --view` composite. Append a fresh view
            // of the just-opened page. Page id is resolved
            // automatically from the open response — agents do not pass
            // it. View is never cached; idempotent open + fresh view.
            if req.flags.contains_key("view") {
                let view_req = Request::new("vs_view")
                    .arg(page_id.clone())
                    .flag_value("session", session_id.clone());
                let view_wire = super::page_ops::handle_view(daemon, &view_req);
                return format!("{open_wire}\n{view_wire}");
            }
            open_wire
        }
        Err(e) => format_daemon_error(&e),
    }
}

pub(super) fn handle_goto(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_goto: missing page id".into()],
        );
    };
    let Some(url) = req.args.get(1).cloned() else {
        return format_error(ErrorCode::BadRequest, vec!["vs_goto: missing url".into()]);
    };
    match daemon.navigate(&session_id, &page_id, &url) {
        Ok(OpenResponse {
            page_id,
            token,
            warnings,
        }) => {
            let mut head = ResponseHead::ok(token);
            head.warnings = warnings;
            let goto_wire = format!("{}{page_id}\n", head.encode());
            // `--view` composite: append a fresh full view of the page
            // after navigating, same as `vs_open --view`.
            if req.flags.contains_key("view") {
                let view_req = Request::new("vs_view")
                    .arg(page_id.clone())
                    .flag_value("session", session_id.clone());
                let view_wire = super::page_ops::handle_view(daemon, &view_req);
                return format!("{goto_wire}\n{view_wire}");
            }
            goto_wire
        }
        Err(e) => format_daemon_error(&e),
    }
}

pub(super) fn handle_close(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_close: missing page id".into()],
        );
    };
    match daemon.close(&session_id, &page_id) {
        Ok(CloseResponse) => ResponseHead::ok(StateToken::ZERO).encode(),
        Err(e) => format_daemon_error(&e),
    }
}
