//! Wire handlers for store-only primitives: extract, mark, annotate,
//! log; plus `parse_target` for `vs_annotate`.

use vs_protocol::{ErrorCode, Ref, Request, ResponseHead, StateToken};
use vs_store::AnnotationTarget;

use super::helpers::{flag_value, format_daemon_error, format_error, require_session};
use crate::daemon::{AnnotateResponse, Daemon, ExtractResponse, LogResponse, MarkResponse};

pub(super) fn handle_extract(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_extract: missing page id".into()],
        );
    };
    let Some(schema) = req.args.get(1).cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_extract: missing schema".into()],
        );
    };
    let Some(before_token) = flag_value(req, "token").and_then(|s| s.parse::<StateToken>().ok())
    else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_extract: --token=<hex> required".into()],
        );
    };
    match daemon.extract(&session_id, &page_id, &schema, before_token) {
        Ok(ExtractResponse { token, records }) => {
            let mut body = String::new();
            for record in records {
                body.push_str(&record.join("\t"));
                body.push('\n');
            }
            format!("{}{body}", ResponseHead::ok(token).encode())
        }
        Err(e) => format_daemon_error(&e),
    }
}

pub(super) fn handle_mark(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_mark: missing page id".into()],
        );
    };
    let r: Ref = match req.args.get(1).map(|s| s.parse::<Ref>()) {
        Some(Ok(r)) => r,
        _ => return format_error(ErrorCode::BadRequest, vec!["vs_mark: bad ref".into()]),
    };
    let Some(name) = req.args.get(2).cloned() else {
        return format_error(ErrorCode::BadRequest, vec!["vs_mark: missing name".into()]);
    };
    let Some(before_token) = flag_value(req, "token").and_then(|s| s.parse::<StateToken>().ok())
    else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_mark: --token=<hex> required".into()],
        );
    };
    match daemon.mark(&session_id, &page_id, r, &name, before_token) {
        Ok(MarkResponse { mark_id, token }) => {
            format!("{}{mark_id}\n", ResponseHead::ok(token).encode())
        }
        Err(e) => format_daemon_error(&e),
    }
}

pub(super) fn handle_annotate(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(target_str) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_annotate: missing target".into()],
        );
    };
    let Some(key) = req.args.get(1).cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_annotate: missing key".into()],
        );
    };
    let value = req.args.get(2).cloned();
    let target = match parse_target(&target_str) {
        Ok(t) => t,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    match daemon.annotate(&session_id, &target, &key, value.as_deref()) {
        Ok(AnnotateResponse { id }) => {
            format!("{}{id}\n", ResponseHead::ok(StateToken::ZERO).encode())
        }
        Err(e) => format_daemon_error(&e),
    }
}

pub(super) fn handle_log(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let page_id = flag_value(req, "page");
    let group_label = flag_value(req, "group");
    let since: Option<i64> = flag_value(req, "since").and_then(|s| s.parse().ok());
    let limit: Option<i64> = flag_value(req, "limit").and_then(|s| s.parse().ok());

    match daemon.log(&session_id, page_id, group_label, since, limit) {
        Ok(LogResponse { rows }) => {
            let mut body = String::new();
            for row in rows {
                use std::fmt::Write as _;
                let _ = writeln!(
                    body,
                    "{}\t{}\t{}\t{}\t{}ms\t{}",
                    row.id,
                    row.primitive,
                    row.page_id.as_deref().unwrap_or(""),
                    row.args_redacted,
                    row.latency_ms,
                    row.error_code.as_deref().unwrap_or("ok"),
                );
            }
            format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
        }
        Err(e) => format_daemon_error(&e),
    }
}

fn parse_target(s: &str) -> std::result::Result<AnnotationTarget, String> {
    if s == "page" {
        return Ok(AnnotationTarget::Page);
    }
    if let Some(rest) = s.strip_prefix("ref:") {
        let r: u32 = rest
            .parse()
            .map_err(|_| format!("annotate: bad ref in target {s:?}"))?;
        return Ok(AnnotationTarget::Ref(r));
    }
    if let Some(rest) = s.strip_prefix("mark:") {
        return Ok(AnnotationTarget::Mark(rest.to_string()));
    }
    Err(format!(
        "annotate: bad target spec {s:?} (use ref:N | mark:NAME | page)"
    ))
}
