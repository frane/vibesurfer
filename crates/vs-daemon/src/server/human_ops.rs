//! Human-in-the-loop handlers: queued prompt input, browser entry forms,
//! the pending queue, live watch frames and page recording.

use vs_protocol::{ErrorCode, Ref, Request, ResponseHead, StateToken};

use super::helpers::{flag_value, format_daemon_error, format_error, require_session};
use crate::daemon::Daemon;

pub(super) fn handle_prompt_input_queue(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_prompt_input_queue: missing page id".into()],
        );
    };
    let Some(r_str) = req.args.get(1) else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_prompt_input_queue: missing ref".into()],
        );
    };
    let Ok(r) = r_str.parse::<Ref>() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_prompt_input_queue: bad ref".into()],
        );
    };
    let Some(message) = req.args.get(2).cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_prompt_input_queue: missing message".into()],
        );
    };
    let Some(token) = flag_value(req, "token") else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_prompt_input_queue: missing --token".into()],
        );
    };
    let secret = req.flags.contains_key("secret");
    let group = flag_value(req, "group");
    // Timeout default 5 min; overridable via --timeout-ms.
    let timeout = match flag_value(req, "timeout-ms").and_then(|s| s.parse::<u64>().ok()) {
        Some(ms) => std::time::Duration::from_millis(ms),
        None => std::time::Duration::from_secs(300),
    };
    match daemon.prompt_input_queue(
        &session_id,
        &page_id,
        r,
        message,
        secret,
        token,
        group,
        timeout,
    ) {
        Ok(after) => ResponseHead::ok(after).encode(),
        Err(e) => format_daemon_error(&e),
    }
}

/// `vs_prompt_form` — enqueue a multi-field form, no parking.
/// Args: `<page> <field>...` where a field is `<ref>:<secret 0|1>:<label>`
/// (label last so it may contain colons). Returns body lines
/// `form\t<id>` and `url\t<entry url>`.
pub(super) fn handle_prompt_form(daemon: &Daemon, req: &Request) -> String {
    if let Err(msg) = require_session(req) {
        return format_error(ErrorCode::BadRequest, vec![msg]);
    }
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_prompt_form: missing page id".into()],
        );
    };
    let Some(token) = flag_value(req, "token") else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_prompt_form: missing --token".into()],
        );
    };
    let mut fields = Vec::new();
    for spec in &req.args[1..] {
        let mut parts = spec.splitn(3, ':');
        let (Some(r_str), Some(secret_str), Some(label)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return format_error(
                ErrorCode::BadRequest,
                vec![format!(
                    "vs_prompt_form: bad field {spec:?} (want <ref>:<0|1>:<label>)"
                )],
            );
        };
        let Ok(r) = r_str.parse::<Ref>() else {
            return format_error(
                ErrorCode::BadRequest,
                vec![format!("vs_prompt_form: bad ref in field {spec:?}")],
            );
        };
        fields.push((r, label.to_string(), secret_str == "1"));
    }
    let group = flag_value(req, "group");
    match daemon.prompt_form_enqueue(&page_id, fields, &token, group) {
        Ok((form_id, url)) => format!(
            "{}form\t{form_id}\nurl\t{url}\n",
            ResponseHead::ok(StateToken([0u8; 8])).encode()
        ),
        Err(e) => format_daemon_error(&e),
    }
}

/// `vs_prompt_form_wait` — park until the form is fulfilled, then
/// fill the refs and return the new state token.
pub(super) fn handle_prompt_form_wait(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(form_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_prompt_form_wait: missing form id".into()],
        );
    };
    let timeout = match flag_value(req, "timeout-ms").and_then(|s| s.parse::<u64>().ok()) {
        Some(ms) => std::time::Duration::from_millis(ms),
        None => std::time::Duration::from_secs(300),
    };
    match daemon.prompt_form_wait(&session_id, &form_id, timeout) {
        Ok(after) => ResponseHead::ok(after).encode(),
        Err(e) => format_daemon_error(&e),
    }
}

/// `vs_pending_url` — mint a browser entry URL for whatever is (or
/// becomes) pending. Body line: `url\t<entry url>`.
pub(super) fn handle_pending_url(daemon: &Daemon, _req: &Request) -> String {
    match daemon.web_entry_url() {
        Ok(url) => format!(
            "{}url\t{url}\n",
            ResponseHead::ok(StateToken([0u8; 8])).encode()
        ),
        Err(e) => format_daemon_error(&e),
    }
}

/// `vs_watch` — mint a live-view URL for a page. Body: `url\t<url>`.
/// `vs_record` start|stop. Args: `<action> <page>`. `--fps` on start.
/// Body: `path\t<ivf path>`.
pub(super) fn handle_record(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let action = req.args.first().map_or("", String::as_str);
    let Some(page_id) = req.args.get(1).cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_record: missing page id".into()],
        );
    };
    let result = match action {
        "start" => {
            let fps = flag_value(req, "fps")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(24);
            // Downscale to `--width` px (default 960), keeping recordings
            // fast to encode and light on disk; `--retina` keeps the full
            // device resolution (bigger + heavier).
            let max_width = if req.flags.contains_key("retina") {
                0
            } else {
                flag_value(req, "width")
                    .and_then(|s| s.parse::<u32>().ok())
                    .filter(|w| *w > 0)
                    .unwrap_or(960)
            };
            let webm = match flag_value(req, "format").as_deref() {
                None | Some("mp4") => false,
                Some("webm") => true,
                Some(f) => {
                    return format_error(
                        ErrorCode::BadRequest,
                        vec![format!("vs_record: bad --format={f:?} (mp4|webm)")],
                    )
                }
            };
            daemon.record_start(&session_id, &page_id, fps, max_width, webm)
        }
        "stop" => daemon.record_stop(&page_id),
        other => {
            return format_error(
                ErrorCode::BadRequest,
                vec![format!("vs_record: unknown action {other:?} (start|stop)")],
            )
        }
    };
    match result {
        Ok(path) => format!(
            "{}path\t{}\n",
            ResponseHead::ok(StateToken([0u8; 8])).encode(),
            path.display()
        ),
        Err(e) => format_daemon_error(&e),
    }
}

pub(super) fn handle_watch(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_watch: missing page id".into()],
        );
    };
    match daemon.watch_url(&session_id, &page_id) {
        Ok(url) => format!(
            "{}url\t{url}\n",
            ResponseHead::ok(StateToken([0u8; 8])).encode()
        ),
        Err(e) => format_daemon_error(&e),
    }
}

/// `vs_frame` — one transient viewport PNG by page id (no session).
/// Body: the on-disk path; the CALLER deletes the file after reading.
/// Plumbing for the MCP live panel and action thumbnails.
pub(super) fn handle_frame(daemon: &Daemon, req: &Request) -> String {
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_frame: missing page id".into()],
        );
    };
    match daemon.live_frame_path(&page_id, 1440) {
        Ok(path) => format!(
            "{}{}\n",
            ResponseHead::ok(StateToken([0u8; 8])).encode(),
            path.display()
        ),
        Err(e) => format_daemon_error(&e),
    }
}

pub(super) fn handle_pending_list(daemon: &Daemon, _req: &Request) -> String {
    use std::fmt::Write as _;
    let entries = daemon.pending_list();
    let mut body = String::new();
    for e in entries {
        // Wire form: `<id>\t<page>\t<ref>\t<secret 0|1>\t<message>`.
        let _ = writeln!(
            body,
            "{}\t{}\t{}\t{}\t{}",
            e.id,
            e.page,
            e.r,
            u8::from(e.secret),
            e.message
        );
    }
    format!(
        "{}{}",
        ResponseHead::ok(StateToken([0u8; 8])).encode(),
        body
    )
}

pub(super) fn handle_pending_fulfill(daemon: &Daemon, req: &Request) -> String {
    let Some(id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_pending_fulfill: missing id".into()],
        );
    };
    let Some(value) = req.args.get(1).cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_pending_fulfill: missing value".into()],
        );
    };
    if daemon.pending_fulfill(&id, value) {
        ResponseHead::ok(StateToken([0u8; 8])).encode()
    } else {
        format_error(
            ErrorCode::NotFound,
            vec![format!("pending entry {id} not found")],
        )
    }
}

pub(super) fn handle_pending_cancel(daemon: &Daemon, req: &Request) -> String {
    let Some(id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_pending_cancel: missing id".into()],
        );
    };
    if daemon.pending_cancel(&id) {
        ResponseHead::ok(StateToken([0u8; 8])).encode()
    } else {
        format_error(
            ErrorCode::NotFound,
            vec![format!("pending entry {id} not found")],
        )
    }
}

pub(super) fn handle_pending_peek(daemon: &Daemon, req: &Request) -> String {
    let Some(id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_pending_peek: missing id".into()],
        );
    };
    match daemon.pending_peek(&id) {
        Some(e) => format!(
            "{}{}\t{}\t{}\t{}\t{}\n",
            ResponseHead::ok(StateToken([0u8; 8])).encode(),
            e.id,
            e.page,
            e.r,
            u8::from(e.secret),
            e.message
        ),
        None => format_error(
            ErrorCode::NotFound,
            vec![format!("pending entry {id} not found")],
        ),
    }
}
