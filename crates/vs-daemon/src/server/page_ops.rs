//! Wire handlers for page operations: view, read, act, find, wait,
//! status.

use std::time::Duration;

use vs_engine_webkit::{
    ActTarget as EngineTarget, Action as EngineAction, WaitCondition as EngineWait,
};
use vs_protocol::{ErrorCode, Op, Ref, Request, ResponseHead, StateToken};

use super::helpers::{flag_value, format_daemon_error, format_error, require_session};
use crate::daemon::{
    ActCall, ActResponse, Daemon, FindResponse, ReadResponse, StatusResponse, ViewResponse,
    WaitResponse,
};
use crate::page_state::ViewForm;
use crate::tokens;

pub(super) fn handle_view(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_view: missing page id".into()],
        );
    };
    let force_full = req.flags.contains_key("full");
    match daemon.view(&session_id, &page_id, force_full) {
        Ok(ViewResponse {
            token,
            form,
            warnings,
        }) => {
            let mut head = ResponseHead::ok(token);
            head.warnings = warnings;
            // M5.7 PR6: ? console_error warning. If new console errors
            // arrived since the previous vs_view for this page, prepend
            // a warning so the agent knows to call `vs_inspect console`.
            let new_errors = count_new_console_errors(daemon, &session_id, &page_id);
            if new_errors > 0 {
                head.warnings.push(vs_protocol::Warning::with_args(
                    vs_protocol::WarningCode::ConsoleError,
                    vec![new_errors.to_string()],
                ));
            }
            let body = match form {
                ViewForm::Full(tree) => tree.encode(),
                ViewForm::Delta(ops) => vs_protocol::delta::encode(&ops),
                ViewForm::NoChange => String::new(),
            };
            let mut wire = format!("{}{body}", head.encode());

            // M5.5 PR5: --layout=N,M composite. Append layout boxes
            // for the requested refs as a second body section
            // separated by a blank line. Missing refs produce
            // `! NOT_FOUND ref=N` inline; they do not abort the view.
            if let Some(refs) = parse_layout_refs(req) {
                let layout_body = build_layout_section(daemon, &session_id, &page_id, &refs);
                wire.push('\n');
                wire.push_str(&layout_body);
            }
            // M5.5 PR6: --read=N composite. Append the full text of
            // one ref's subtree as another body section. Unknown ref
            // produces ! NOT_FOUND; the view still succeeds.
            if let Some(r) = parse_read_ref(req) {
                let read_body = build_read_section(daemon, &session_id, &page_id, r);
                wire.push('\n');
                wire.push_str(&read_body);
            }
            wire
        }
        Err(e) => format_daemon_error(&e),
    }
}

/// Count console-error entries that arrived since the *previous*
/// `vs_view` audit row for `page_id`. Used to emit the
/// `? console_error <count>` warning on every view.
///
/// "Previous" means the second-most-recent `vs_view` row, since the
/// most recent is the one that just landed inside `daemon.view()`'s
/// audit_call. Returns 0 if there is no prior view (i.e., this is the
/// first one for the page).
fn count_new_console_errors(daemon: &Daemon, session_id: &str, page_id: &str) -> u32 {
    let prior_finished_at = {
        let rows = daemon
            .audit_log(&vs_store::ActionFilter {
                page_id: Some(page_id.to_string()),
                ..Default::default()
            })
            .unwrap_or_default();
        let mut view_rows: Vec<&vs_store::Action> =
            rows.iter().filter(|r| r.primitive == "vs_view").collect();
        view_rows.sort_by_key(|r| r.finished_at);
        // Drop the most recent (this call's row). If no prior, return 0.
        if view_rows.len() < 2 {
            return 0;
        }
        view_rows[view_rows.len() - 2].finished_at
    };
    let entries = daemon
        .inspect_console(session_id, page_id)
        .unwrap_or_default();
    entries
        .iter()
        .filter(|e| matches!(e.level, vs_engine_webkit::inspector::ConsoleLevel::Error))
        .filter(|e| {
            e.timestamp
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64)
                > prior_finished_at
        })
        .count() as u32
}
/// Parse the `--layout=<comma-separated-refs>` flag. Returns `None` if
/// the flag is absent.
fn parse_layout_refs(req: &Request) -> Option<Vec<Ref>> {
    let raw = req.flags.get("layout")?.as_ref()?;
    let mut out = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Ok(r) = part.parse::<Ref>() {
            out.push(r);
        }
    }
    Some(out)
}

/// Parse the `--read=<ref>` flag.
fn parse_read_ref(req: &Request) -> Option<Ref> {
    req.flags.get("read")?.as_ref()?.parse::<Ref>().ok()
}

/// Render the read body (full text of `r`'s subtree) or a NOT_FOUND
/// line if the ref isn't in the current tree.
fn build_read_section(daemon: &Daemon, session_id: &str, page_id: &str, r: Ref) -> String {
    match daemon.read(session_id, page_id, r) {
        Ok(ReadResponse { body, .. }) => body,
        Err(_) => format!("! NOT_FOUND ref={}\n", r.0),
    }
}
/// either a successful box line or `! NOT_FOUND ref=N`.
fn build_layout_section(daemon: &Daemon, session_id: &str, page_id: &str, refs: &[Ref]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let layout_result = daemon.layout(session_id, page_id, refs.to_vec());
    let boxes = match layout_result {
        Ok(crate::daemon::LayoutResponse { boxes, .. }) => boxes,
        Err(e) => {
            // If the layout call itself errored (e.g. unknown page),
            // emit one error line covering the whole batch.
            let _ = writeln!(out, "! {} layout error", e.wire().0);
            return out;
        }
    };
    let returned: std::collections::HashSet<u32> = boxes.iter().map(|b| b.r.0).collect();
    for r in refs {
        if let Some(b) = boxes.iter().find(|b| b.r == *r) {
            let _ = writeln!(
                out,
                "{} x={} y={} w={} h={} visible={} z={}",
                b.r, b.x, b.y, b.width, b.height, b.visible, b.z_index
            );
        } else if !returned.contains(&r.0) {
            let _ = writeln!(out, "! NOT_FOUND ref={}", r.0);
        }
    }
    out
}

pub(super) fn handle_read(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_read: missing page id".into()],
        );
    };
    let r: Ref = match req.args.get(1).map(|s| s.parse()) {
        Some(Ok(r)) => r,
        _ => return format_error(ErrorCode::BadRequest, vec!["vs_read: bad ref".into()]),
    };
    match daemon.read(&session_id, &page_id, r) {
        Ok(ReadResponse { token, body }) => {
            format!("{}{body}", ResponseHead::ok(token).encode())
        }
        Err(e) => format_daemon_error(&e),
    }
}

pub(super) fn handle_act(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_act: missing page id".into()],
        );
    };
    let r: Ref = match req.args.get(1).map(|s| s.parse()) {
        Some(Ok(r)) => r,
        _ => return format_error(ErrorCode::BadRequest, vec!["vs_act: bad ref".into()]),
    };
    let op: Op = match req.args.get(2).map(|s| s.parse()) {
        Some(Ok(o)) => o,
        _ => return format_error(ErrorCode::BadRequest, vec!["vs_act: bad op".into()]),
    };
    let val = req.args.get(3).cloned();

    let action = match (op, val) {
        (Op::Click, _) => EngineAction::Click,
        (Op::Submit, _) => EngineAction::Submit,
        (Op::Hover, _) => EngineAction::Hover,
        (Op::Focus, _) => EngineAction::Focus,
        (Op::Scroll, _) => EngineAction::Scroll,
        (Op::Fill, Some(v)) => EngineAction::Fill { value: v },
        (Op::Key, Some(v)) => EngineAction::Key { chord: v },
        (Op::Fill | Op::Key, None) => {
            return format_error(
                ErrorCode::BadRequest,
                vec![format!("vs_act: {op} requires a value")],
            );
        }
    };

    let Some(token_str) = flag_value(req, "token") else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_act: --token required".into()],
        );
    };
    let Ok(before_token) = token_str.parse::<StateToken>() else {
        return format_error(ErrorCode::BadRequest, vec!["vs_act: bad --token".into()]);
    };

    let group_label = flag_value(req, "group");
    let target = EngineTarget::Ref(r);
    let args_vec: Vec<String> = req.args.clone();
    let args_hash = tokens::args_hash("vs_act", &args_vec);
    let args_redacted = crate::redact::redact_args(
        &req.args,
        &req.flags
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>(),
    );

    // Save copies of ids for the optional --view composite below.
    let session_for_view = session_id.clone();
    let page_for_view = page_id.clone();
    let mode = flag_value(req, "mode")
        .and_then(|s| vs_engine_webkit::engine::InputMode::parse(&s))
        .unwrap_or(vs_engine_webkit::engine::InputMode::Careful);
    let act_outcome = daemon.act(ActCall {
        session_id,
        page_id,
        target,
        action,
        before_token,
        args_hash,
        args_redacted,
        mode,
        group_label,
    });

    let act_wire = match act_outcome {
        Ok(ActResponse {
            token,
            form,
            warnings,
        }) => {
            let mut head = ResponseHead::ok(token);
            head.warnings = warnings;
            let body = match form {
                ViewForm::Full(tree) => tree.encode(),
                ViewForm::Delta(ops) => vs_protocol::delta::encode(&ops),
                ViewForm::NoChange => String::new(),
            };
            format!("{}{body}", head.encode())
        }

        Err(e) => return format_daemon_error(&e),
    };

    // M5.5 PR4: --view composite. Skipped on any act error (we
    // returned early above). An idempotency-cache hit on the act
    // still runs a fresh view (the act_wire already carries the
    // ? idempotent_hit warning before its envelope).
    if req.flags.contains_key("view") {
        let view_req = Request::new("vs_view")
            .arg(page_for_view)
            .flag_value("session", session_for_view);
        let view_wire = handle_view(daemon, &view_req);
        return format!("{act_wire}\n{view_wire}");
    }
    act_wire
}

pub(super) fn handle_find(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(query) = req.args.first().cloned() else {
        return format_error(ErrorCode::BadRequest, vec!["vs_find: missing query".into()]);
    };
    match daemon.find(&session_id, &query) {
        Ok(FindResponse { hits }) => {
            let mut body = String::new();
            for h in hits {
                use std::fmt::Write as _;
                let _ = writeln!(body, "{} {} {} {}", h.page_id, h.r, h.role, h.label);
            }
            format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
        }
        Err(e) => format_daemon_error(&e),
    }
}

pub(super) fn handle_wait(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_wait: missing page id".into()],
        );
    };
    let Some(cond_name) = req.args.get(1).map(String::as_str) else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_wait: missing condition".into()],
        );
    };
    let cond = match cond_name {
        "stable" => EngineWait::Stable,
        "net-idle" => EngineWait::NetIdle,
        "ref" => match req.args.get(2).map(|s| s.parse::<Ref>()) {
            Some(Ok(r)) => EngineWait::RefAppears(r),
            _ => return format_error(ErrorCode::BadRequest, vec!["vs_wait ref: bad ref".into()]),
        },
        "gone" => match req.args.get(2).map(|s| s.parse::<Ref>()) {
            Some(Ok(r)) => EngineWait::RefGone(r),
            _ => return format_error(ErrorCode::BadRequest, vec!["vs_wait gone: bad ref".into()]),
        },
        "text" => match req.args.get(2) {
            Some(t) => EngineWait::Text(t.clone()),
            None => {
                return format_error(
                    ErrorCode::BadRequest,
                    vec!["vs_wait text: missing text".into()],
                );
            }
        },
        "token-change" => EngineWait::TokenChange,
        other => {
            return format_error(
                ErrorCode::BadRequest,
                vec![format!("vs_wait: unknown condition {other}")],
            );
        }
    };
    let budget_ms: u64 = flag_value(req, "timeout")
        .and_then(|s| s.trim_end_matches("ms").parse().ok())
        .unwrap_or(5000);
    let budget = Duration::from_millis(budget_ms);

    match daemon.wait(&session_id, &page_id, cond, budget) {
        Ok(WaitResponse { token }) => {
            let wait_wire = ResponseHead::ok(token).encode();
            // M5.5 PR3: --view composite. View token is post-wait;
            // skipped on TIMEOUT (which lands in the Err branch below).
            if req.flags.contains_key("view") {
                let view_req = Request::new("vs_view")
                    .arg(page_id.clone())
                    .flag_value("session", session_id.clone());
                let view_wire = handle_view(daemon, &view_req);
                return format!("{wait_wire}\n{view_wire}");
            }
            wait_wire
        }
        Err(e) => format_daemon_error(&e),
    }
}

pub(super) fn handle_status(daemon: &Daemon, req: &Request) -> String {
    let session = flag_value(req, "session");
    match daemon.status(session.as_deref()) {
        Ok(StatusResponse { body }) => {
            format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
        }
        Err(e) => format_daemon_error(&e),
    }
}
