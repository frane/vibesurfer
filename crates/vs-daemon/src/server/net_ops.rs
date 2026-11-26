//! Wire handlers for the intercept primitives:
//!
//! - `vs_net_poll [--wait-ms=N]` drains the session's event lines.
//! - `vs_net_route on|off` holds every request (or stops holding).
//! - `vs_net_continue <id> [--url=] [--method=] [--headers=] [--body-b64=]`
//! - `vs_net_fulfill <id> [--status=] [--headers=] [--body-b64=]`
//! - `vs_net_abort <id> [--error=]`
//! - `vs_net_fetch <id> [overrides as continue]` returns the response.
//! - `vs_net_body <id>` returns a kept response body.
//!
//! `--headers` is `name: value` lines joined by newlines (the wire
//! escapes them); when given it replaces the whole header list.

use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use vs_protocol::{ErrorCode, Request, ResponseHead, StateToken};

use super::helpers::{flag_value, format_daemon_error, format_error, require_session};
use crate::daemon::Daemon;
use crate::net::{
    parse_header_lines, render_response, Decision, FetchError, Overrides, SessionNet,
};

/// Longest `--wait-ms` a poll may block.
const MAX_WAIT: Duration = Duration::from_secs(30);

fn ok(body: &str) -> String {
    format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
}

fn bad(msg: impl Into<String>) -> String {
    format_error(ErrorCode::BadRequest, vec![msg.into()])
}

/// Session proxy plus the numeric request id in `args[0]`, or the
/// wire error to send instead.
fn net_and_id(
    daemon: &Daemon,
    req: &Request,
) -> Result<(std::sync::Arc<SessionNet>, Option<u64>), String> {
    let session_id = require_session(req).map_err(bad)?;
    let net = daemon
        .net_for(&session_id)
        .map_err(|e| format_daemon_error(&e))?;
    let with_id = !matches!(req.primitive.as_str(), "vs_net_poll" | "vs_net_route");
    let id = match req.args.first().filter(|_| with_id) {
        Some(a) => Some(
            a.parse::<u64>()
                .map_err(|_| bad(format!("{}: bad request id {a}", req.primitive)))?,
        ),
        None => None,
    };
    Ok((net, id))
}

fn need_id(req: &Request, id: Option<u64>) -> Result<u64, String> {
    id.ok_or_else(|| bad(format!("{}: missing request id", req.primitive)))
}

fn body_flag(req: &Request) -> Result<Option<Vec<u8>>, String> {
    flag_value(req, "body-b64")
        .map(|b| {
            B64.decode(b.trim())
                .map_err(|e| bad(format!("--body-b64: {e}")))
        })
        .transpose()
}

fn overrides(req: &Request) -> Result<Overrides, String> {
    Ok(Overrides {
        url: flag_value(req, "url"),
        method: flag_value(req, "method"),
        headers: flag_value(req, "headers").map(|h| parse_header_lines(&h)),
        body: body_flag(req)?,
    })
}

fn not_held(id: u64) -> String {
    format_error(ErrorCode::NotFound, vec![format!("request={id}")])
}

pub(super) fn handle_poll(daemon: &Daemon, req: &Request) -> String {
    let (net, _) = match net_and_id(daemon, req) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let wait = flag_value(req, "wait-ms")
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(Duration::ZERO, Duration::from_millis)
        .min(MAX_WAIT);
    ok(&net.poll(wait))
}

pub(super) fn handle_route(daemon: &Daemon, req: &Request) -> String {
    let (net, _) = match net_and_id(daemon, req) {
        Ok(v) => v,
        Err(e) => return e,
    };
    match req.args.first().map(String::as_str) {
        Some("on") => net.set_route(true),
        Some("off") => net.set_route(false),
        _ => return bad("vs_net_route: on|off"),
    }
    ok("")
}

pub(super) fn handle_continue(daemon: &Daemon, req: &Request) -> String {
    let run = || -> Result<String, String> {
        let (net, id) = net_and_id(daemon, req)?;
        let id = need_id(req, id)?;
        let o = overrides(req)?;
        if net.decide(id, Decision::Continue(o)) {
            Ok(ok(""))
        } else {
            Err(not_held(id))
        }
    };
    run().unwrap_or_else(|e| e)
}

pub(super) fn handle_fulfill(daemon: &Daemon, req: &Request) -> String {
    let run = || -> Result<String, String> {
        let (net, id) = net_and_id(daemon, req)?;
        let id = need_id(req, id)?;
        let status = match flag_value(req, "status") {
            Some(s) => s
                .parse::<u16>()
                .ok()
                .filter(|n| (100..=999).contains(n))
                .ok_or_else(|| bad(format!("--status: {s}")))?,
            None => 200,
        };
        let decision = Decision::Fulfill {
            status,
            headers: flag_value(req, "headers")
                .map(|h| parse_header_lines(&h))
                .unwrap_or_default(),
            body: body_flag(req)?.unwrap_or_default(),
        };
        if net.decide(id, decision) {
            Ok(ok(""))
        } else {
            Err(not_held(id))
        }
    };
    run().unwrap_or_else(|e| e)
}

pub(super) fn handle_abort(daemon: &Daemon, req: &Request) -> String {
    let run = || -> Result<String, String> {
        let (net, id) = net_and_id(daemon, req)?;
        let id = need_id(req, id)?;
        let error = flag_value(req, "error").unwrap_or_else(|| "failed".into());
        if net.decide(id, Decision::Abort(error)) {
            Ok(ok(""))
        } else {
            Err(not_held(id))
        }
    };
    run().unwrap_or_else(|e| e)
}

/// `vs_net_dialog <id> accept|dismiss [--text=T]`: answer a `dialog`
/// event. A prompt accepted without `--text` gets its default.
pub(super) fn handle_dialog(daemon: &Daemon, req: &Request) -> String {
    let run = || -> Result<String, String> {
        let (_net, id) = net_and_id(daemon, req)?;
        let id = need_id(req, id)?;
        let accept = match req.args.get(1).map(String::as_str) {
            Some("accept") => true,
            Some("dismiss") => false,
            _ => return Err(bad("vs_net_dialog: <id> accept|dismiss")),
        };
        match daemon.answer_dialog(id, accept, flag_value(req, "text")) {
            Some(true) => Ok(ok("")),
            Some(false) => Err(format_error(
                ErrorCode::NotFound,
                vec![format!(
                    "dialog {id} is not open: unknown, already answered, or dismissed after {} s",
                    vs_engine_webkit::engine::DIALOG_TIMEOUT.as_secs()
                )],
            )),
            None => Err(format_error(
                ErrorCode::EngineUnsupported,
                vec!["vs_net_dialog".into()],
            )),
        }
    };
    run().unwrap_or_else(|e| e)
}

pub(super) fn handle_fetch(daemon: &Daemon, req: &Request) -> String {
    let run = || -> Result<String, String> {
        let (net, id) = net_and_id(daemon, req)?;
        let id = need_id(req, id)?;
        let o = overrides(req)?;
        match net.fetch(id, &o) {
            Ok(f) => Ok(ok(&render_response(id, &f))),
            Err(FetchError::NotHeld) => Err(not_held(id)),
            Err(FetchError::Network(e)) => Err(format_error(
                ErrorCode::NotFound,
                vec![format!("fetch={id}"), e.replace(['\r', '\n'], " ")],
            )),
        }
    };
    run().unwrap_or_else(|e| e)
}

pub(super) fn handle_body(daemon: &Daemon, req: &Request) -> String {
    let run = || -> Result<String, String> {
        let (net, id) = net_and_id(daemon, req)?;
        let id = need_id(req, id)?;
        net.body(id)
            .map(|b| ok(&format!("body {}\n", B64.encode(b))))
            .ok_or_else(|| format_error(ErrorCode::NotFound, vec![format!("body={id}")]))
    };
    run().unwrap_or_else(|e| e)
}
