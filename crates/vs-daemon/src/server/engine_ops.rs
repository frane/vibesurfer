//! Wire handlers for engine-backed primitives: skill, capture,
//! viewport, layout, auth.

use vs_engine_webkit::{CaptureScope, Viewport};
use vs_protocol::{ErrorCode, Ref, Request, ResponseHead, StateToken};

use super::helpers::{flag_value, format_daemon_error, format_error, require_session};
use crate::daemon::{
    AuthListResponse, AuthLoadResponse, AuthSaveResponse, CaptureResponse, Daemon, LayoutResponse,
    SkillListResponse, SkillShowResponse, ViewportResponse,
};

/// Default body truncation for `vs_inspect request` output. Override
/// with `--full`.
const REQUEST_BODY_TRUNCATE: usize = 4096;

pub(super) fn handle_skill(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let sub = req.args.first().map_or("list", String::as_str);
    match sub {
        "list" => match daemon.skill_list(&session_id) {
            Ok(SkillListResponse { names }) => {
                let mut body = String::new();
                for n in names {
                    body.push_str(&n);
                    body.push('\n');
                }
                format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
            }
            Err(e) => format_daemon_error(&e),
        },
        "show" => {
            let Some(name) = req.args.get(1) else {
                return format_error(
                    ErrorCode::BadRequest,
                    vec!["vs_skill show: missing name".into()],
                );
            };
            match daemon.skill_show(&session_id, name) {
                Ok(SkillShowResponse { body }) => {
                    format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
                }
                Err(e) => format_daemon_error(&e),
            }
        }
        other => format_error(
            ErrorCode::BadRequest,
            vec![format!(
                "vs_skill: unknown subcommand `{other}` (use list|show; run lands in M6)"
            )],
        ),
    }
}

pub(super) fn handle_capture(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_capture: missing page id".into()],
        );
    };
    let scope = if req.flags.contains_key("full-page") {
        CaptureScope::FullPage
    } else if let Some(r_str) = req.args.get(1) {
        match r_str.parse::<Ref>() {
            Ok(r) => CaptureScope::Element(r),
            Err(_) => {
                return format_error(ErrorCode::BadRequest, vec!["vs_capture: bad ref".into()])
            }
        }
    } else {
        CaptureScope::Viewport
    };
    match daemon.capture(&session_id, &page_id, scope) {
        Ok(CaptureResponse { path, token }) => {
            format!("{}{}\n", ResponseHead::ok(token).encode(), path.display())
        }
        Err(e) => format_daemon_error(&e),
    }
}

pub(super) fn handle_viewport(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_viewport: missing page id".into()],
        );
    };
    let Some(spec) = req.args.get(1).cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_viewport: missing preset|WxH".into()],
        );
    };
    let dpr: u32 = flag_value(req, "dpr")
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let Some(viewport) = parse_viewport_spec(&spec, dpr) else {
        return format_error(
            ErrorCode::BadRequest,
            vec![format!(
                "vs_viewport: bad spec {spec:?} (use preset name or WxH)"
            )],
        );
    };
    match daemon.viewport(&session_id, &page_id, viewport) {
        Ok(ViewportResponse { token, warnings }) => {
            let mut head = ResponseHead::ok(token);
            head.warnings = warnings;
            head.encode()
        }
        Err(e) => format_daemon_error(&e),
    }
}

pub(super) fn handle_layout(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_layout: missing page id".into()],
        );
    };
    let mut refs = Vec::new();
    for raw in req.args.iter().skip(1) {
        match raw.parse::<Ref>() {
            Ok(r) => refs.push(r),
            Err(_) => {
                return format_error(
                    ErrorCode::BadRequest,
                    vec![format!("vs_layout: bad ref {raw:?}")],
                );
            }
        }
    }
    if refs.is_empty() {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_layout: at least one ref required".into()],
        );
    }
    match daemon.layout(&session_id, &page_id, refs) {
        Ok(LayoutResponse { boxes, token }) => {
            let mut body = String::new();
            for b in boxes {
                use std::fmt::Write as _;
                let _ = writeln!(
                    body,
                    "{} x={} y={} w={} h={} visible={} z={}",
                    b.r, b.x, b.y, b.width, b.height, b.visible, b.z_index
                );
            }
            format!("{}{body}", ResponseHead::ok(token).encode())
        }
        Err(e) => format_daemon_error(&e),
    }
}

pub(super) fn handle_auth(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(sub) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_auth: missing save|load|list|clear".into()],
        );
    };
    match sub.as_str() {
        "list" => match daemon.auth_list(&session_id) {
            Ok(AuthListResponse { names }) => {
                let mut body = String::new();
                for n in names {
                    body.push_str(&n);
                    body.push('\n');
                }
                format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
            }
            Err(e) => format_daemon_error(&e),
        },
        "save" => {
            let Some(page_id) = req.args.get(1).cloned() else {
                return format_error(
                    ErrorCode::BadRequest,
                    vec!["vs_auth save: missing page id".into()],
                );
            };
            let Some(name) = req.args.get(2).cloned() else {
                return format_error(
                    ErrorCode::BadRequest,
                    vec!["vs_auth save: missing name".into()],
                );
            };
            match daemon.auth_save(&session_id, &page_id, &name) {
                Ok(AuthSaveResponse { name }) => {
                    format!("{}{name}\n", ResponseHead::ok(StateToken::ZERO).encode())
                }
                Err(e) => format_daemon_error(&e),
            }
        }
        "load" => {
            let Some(page_id) = req.args.get(1).cloned() else {
                return format_error(
                    ErrorCode::BadRequest,
                    vec!["vs_auth load: missing page id".into()],
                );
            };
            let Some(name) = req.args.get(2).cloned() else {
                return format_error(
                    ErrorCode::BadRequest,
                    vec!["vs_auth load: missing name".into()],
                );
            };
            match daemon.auth_load(&session_id, &page_id, &name) {
                Ok(AuthLoadResponse { token, warnings }) => {
                    let mut head = ResponseHead::ok(token);
                    head.warnings = warnings;
                    head.encode()
                }
                Err(e) => format_daemon_error(&e),
            }
        }
        "clear" => {
            let Some(name) = req.args.get(1).cloned() else {
                return format_error(
                    ErrorCode::BadRequest,
                    vec!["vs_auth clear: missing name".into()],
                );
            };
            match daemon.auth_clear(&session_id, &name) {
                Ok(_) => ResponseHead::ok(StateToken::ZERO).encode(),
                Err(e) => format_daemon_error(&e),
            }
        }
        other => format_error(
            ErrorCode::BadRequest,
            vec![format!(
                "vs_auth: unknown subcommand `{other}` (use save|load|list|clear)"
            )],
        ),
    }
}

fn parse_viewport_spec(spec: &str, dpr: u32) -> Option<Viewport> {
    if let Some(v) = Viewport::preset(spec) {
        return Some(Viewport::new(v.width, v.height, dpr));
    }
    let (w_str, h_str) = spec.split_once('x')?;
    let w: u32 = w_str.parse().ok()?;
    let h: u32 = h_str.parse().ok()?;
    Some(Viewport::new(w, h, dpr))
}

// =============================================================================
// M5.7 PR1: vs_inspect — kind-dispatch shell.
// =============================================================================

pub(super) fn handle_inspect(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(kind) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_inspect: missing kind".into()],
        );
    };
    let Some(page_id) = flag_value(req, "page").or_else(|| req.args.get(1).cloned()) else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_inspect: missing --page=<id> (or page positional)".into()],
        );
    };
    match kind.as_str() {
        "console" => render_console(daemon, &session_id, &page_id, req),
        "network" => render_network(daemon, &session_id, &page_id, req),
        "request" => render_request(daemon, &session_id, &page_id, req),
        "eval" => render_eval(daemon, &session_id, &page_id, req),
        "storage" => render_storage(daemon, &session_id, &page_id, req),
        "scripts" => render_scripts(daemon, &session_id, &page_id),
        "script" => render_script(daemon, &session_id, &page_id, req),
        "dom" => render_dom(daemon, &session_id, &page_id, req),
        "performance" => render_performance(daemon, &session_id, &page_id),
        other => format_error(ErrorCode::UnknownKind, vec![format!("vs_inspect: {other}")]),
    }
}

/// Resolve `--since=<token>` to the epoch second when that token was
/// last issued as an `after_token` for `page_id`. Returns `None` if
/// the flag is absent or the token has never been issued for this page.
/// We reuse the existing `actions` table — every primitive call writes
/// `after_token` + `finished_at`, which is exactly the canonical
/// "when this token was first seen by the agent" signal.
fn since_seconds(daemon: &Daemon, page_id: &str, req: &Request) -> Option<i64> {
    let token = flag_value(req, "since")?;
    let rows = daemon
        .audit_log(&vs_store::ActionFilter {
            page_id: Some(page_id.to_string()),
            ..Default::default()
        })
        .ok()?;
    rows.iter()
        .filter(|r| r.after_token.as_deref() == Some(token.as_str()))
        .map(|r| r.finished_at)
        .max()
}

fn parse_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn status_matches(status: &vs_engine_webkit::inspector::NetworkStatus, filter: &str) -> bool {
    use vs_engine_webkit::inspector::NetworkStatus;
    let f = filter.trim().to_ascii_lowercase();
    match status {
        NetworkStatus::Code(c) => {
            if let Ok(target) = f.parse::<u16>() {
                return *c == target;
            }
            if f.len() == 3 && f.ends_with("xx") {
                if let Some(class_digit) = f.chars().next().and_then(|c| c.to_digit(10)) {
                    return u32::from(*c / 100) == class_digit;
                }
            }
            false
        }
        NetworkStatus::Pending => f == "pending",
        NetworkStatus::Abort => f == "abort",
        NetworkStatus::Cors => f == "cors",
        NetworkStatus::Blocked => f == "blocked",
    }
}

#[allow(clippy::cast_possible_wrap)]
fn render_console(daemon: &Daemon, session_id: &str, page_id: &str, req: &Request) -> String {
    use std::fmt::Write as _;
    let entries = match daemon.inspect_console(session_id, page_id) {
        Ok(e) => e,
        Err(e) => return format_daemon_error(&e),
    };
    let level_filter: Option<Vec<vs_engine_webkit::inspector::ConsoleLevel>> =
        flag_value(req, "level").map(|raw| {
            parse_csv(&raw)
                .iter()
                .filter_map(|s| vs_engine_webkit::inspector::ConsoleLevel::parse(s))
                .collect()
        });
    let since = since_seconds(daemon, page_id, req);
    let limit: usize = flag_value(req, "limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);

    let mut body = String::new();
    let mut emitted = 0usize;
    for e in entries {
        if let Some(filter) = &level_filter {
            if !filter.contains(&e.level) {
                continue;
            }
        }
        let ts = e
            .timestamp
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Some(since_secs) = since {
            if ts < since_secs {
                continue;
            }
        }
        if emitted >= limit {
            break;
        }
        let _ = writeln!(body, "{ts} {} {}", e.level.as_str(), e.message);
        if let Some(stack) = &e.stack {
            for line in stack.lines() {
                let _ = writeln!(body, "  {line}");
            }
        }
        emitted += 1;
    }
    format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
}

#[allow(clippy::cast_possible_wrap)]
fn render_network(daemon: &Daemon, session_id: &str, page_id: &str, req: &Request) -> String {
    use std::fmt::Write as _;
    let entries = match daemon.inspect_network(session_id, page_id) {
        Ok(e) => e,
        Err(e) => return format_daemon_error(&e),
    };
    let methods: Option<Vec<String>> = flag_value(req, "method").map(|raw| {
        parse_csv(&raw)
            .into_iter()
            .map(|s| s.to_ascii_uppercase())
            .collect()
    });
    let status_filters: Option<Vec<String>> = flag_value(req, "status").map(|raw| parse_csv(&raw));
    let url_contains: Option<String> = flag_value(req, "url-contains");
    let since = since_seconds(daemon, page_id, req);
    let limit: usize = flag_value(req, "limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);

    let mut body = String::new();
    let mut emitted = 0usize;
    for e in entries {
        if let Some(ms) = &methods {
            if !ms.contains(&e.method.to_ascii_uppercase()) {
                continue;
            }
        }
        if let Some(sf) = &status_filters {
            if !sf.iter().any(|s| status_matches(&e.status, s)) {
                continue;
            }
        }
        if let Some(uc) = &url_contains {
            if !e.url.contains(uc.as_str()) {
                continue;
            }
        }
        let ts = e
            .timestamp
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Some(since_secs) = since {
            if ts < since_secs {
                continue;
            }
        }
        if emitted >= limit {
            break;
        }
        let _ = writeln!(
            body,
            "n_{} {} {} {} {} {}",
            e.seq,
            e.method,
            e.status.as_str(),
            e.latency_ms.unwrap_or(0),
            e.url,
            e.size
        );
        emitted += 1;
    }
    format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
}

fn is_sensitive_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "authorization" | "cookie" | "set-cookie" | "x-api-key" | "x-auth-token"
    )
}

fn render_request(daemon: &Daemon, session_id: &str, page_id: &str, req: &Request) -> String {
    use std::fmt::Write as _;
    let seq_str = req.args.get(2).cloned().or_else(|| flag_value(req, "id"));
    let Some(seq_str) = seq_str else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_inspect request: missing <id> (n_<seq> or numeric)".into()],
        );
    };
    let seq: u64 = match seq_str.strip_prefix("n_").unwrap_or(&seq_str).parse() {
        Ok(s) => s,
        Err(_) => {
            return format_error(
                ErrorCode::BadRequest,
                vec![format!("vs_inspect request: bad id {seq_str:?}")],
            );
        }
    };
    let detail = match daemon.inspect_request(session_id, page_id, seq) {
        Ok(Some(d)) => d,
        Ok(None) => return format_error(ErrorCode::NotFound, vec![format!("request={seq_str}")]),
        Err(e) => return format_daemon_error(&e),
    };
    let unsafe_log = req.flags.contains_key("unsafe-log");
    let truncate = !req.flags.contains_key("full");
    let mut body = String::new();
    let _ = writeln!(body, "> {} {}", detail.method, detail.url);
    for h in &detail.request_headers {
        let v = if !unsafe_log && is_sensitive_header(&h.name) {
            "***"
        } else {
            &h.value
        };
        let _ = writeln!(body, "> {}: {v}", h.name);
    }
    if let Some(b) = &detail.request_body {
        let _ = writeln!(body, ">");
        let trunc = if truncate && b.len() > REQUEST_BODY_TRUNCATE {
            format!("{}... (len={})", &b[..REQUEST_BODY_TRUNCATE], b.len())
        } else {
            b.clone()
        };
        for line in trunc.lines() {
            let _ = writeln!(body, "> {line}");
        }
    }
    let _ = writeln!(body, "< {}", detail.status.as_str());
    for h in &detail.response_headers {
        let v = if !unsafe_log && is_sensitive_header(&h.name) {
            "***"
        } else {
            &h.value
        };
        let _ = writeln!(body, "< {}: {v}", h.name);
    }
    if let Some(b) = &detail.response_body {
        let _ = writeln!(body, "<");
        let trunc = if truncate && b.len() > REQUEST_BODY_TRUNCATE {
            format!("{}... (len={})", &b[..REQUEST_BODY_TRUNCATE], b.len())
        } else {
            b.clone()
        };
        for line in trunc.lines() {
            let _ = writeln!(body, "< {line}");
        }
    }
    format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
}

fn render_eval(daemon: &Daemon, session_id: &str, page_id: &str, req: &Request) -> String {
    use vs_engine_webkit::inspector::EvalResult;
    let Some(expr) = req.args.get(2).cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_inspect eval: missing expression".into()],
        );
    };
    let max: usize = flag_value(req, "max")
        .and_then(|s| s.parse().ok())
        .unwrap_or(4096);
    let truncate = !req.flags.contains_key("full");
    match daemon.inspect_eval(session_id, page_id, &expr) {
        Ok(EvalResult::Ok { value, js_type }) => {
            let len = value.len();
            let body = if truncate && len > max {
                format!("result={}...\ntype={js_type} len={len}\n", &value[..max])
            } else {
                format!("result={value}\ntype={js_type} len={len}\n")
            };
            format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
        }
        Ok(EvalResult::Thrown { kind, message }) => {
            format_error(ErrorCode::EvalError, vec![kind, message])
        }
        Ok(EvalResult::Syntax { message }) => format_error(ErrorCode::EvalSyntax, vec![message]),
        Err(e) => format_daemon_error(&e),
    }
}

fn render_storage(daemon: &Daemon, session_id: &str, page_id: &str, req: &Request) -> String {
    use std::fmt::Write as _;
    use vs_engine_webkit::inspector::StorageScope;
    let Some(scope_arg) = req.args.get(2).cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_inspect storage: missing scope (cookies|local|session|indexeddb)".into()],
        );
    };
    let Some(scope) = StorageScope::parse(&scope_arg) else {
        return format_error(
            ErrorCode::BadRequest,
            vec![format!("vs_inspect storage: unknown scope {scope_arg:?}")],
        );
    };
    let unsafe_log = req.flags.contains_key("unsafe-log");
    let max: usize = flag_value(req, "max")
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let truncate = !req.flags.contains_key("full");
    let entries = match daemon.inspect_storage(session_id, page_id, scope) {
        Ok(e) => e,
        Err(e) => return format_daemon_error(&e),
    };
    let mut body = String::new();
    for entry in entries {
        let value = if entry.sensitive && !unsafe_log {
            "***".to_string()
        } else if truncate && entry.value.len() > max {
            format!("{}...", &entry.value[..max])
        } else {
            entry.value.clone()
        };
        if entry.flags.is_empty() {
            let _ = writeln!(body, "{}={value}", entry.key);
        } else {
            let _ = writeln!(body, "{}={value} {}", entry.key, entry.flags.join(" "));
        }
    }
    format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
}

fn render_scripts(daemon: &Daemon, session_id: &str, page_id: &str) -> String {
    use std::fmt::Write as _;
    let entries = match daemon.inspect_scripts(session_id, page_id) {
        Ok(e) => e,
        Err(e) => return format_daemon_error(&e),
    };
    let mut body = String::new();
    for s in entries {
        let _ = writeln!(
            body,
            "s_{} {} {} {}",
            s.seq,
            s.source,
            s.size,
            s.state.as_str()
        );
    }
    format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
}

fn render_script(daemon: &Daemon, session_id: &str, page_id: &str, req: &Request) -> String {
    let seq_arg = req.args.get(2).cloned();
    let Some(seq_arg) = seq_arg else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_inspect script: missing <id>".into()],
        );
    };
    let seq: u64 = match seq_arg.strip_prefix("s_").unwrap_or(&seq_arg).parse() {
        Ok(n) => n,
        Err(_) => {
            return format_error(
                ErrorCode::BadRequest,
                vec![format!("vs_inspect script: bad id {seq_arg:?}")],
            );
        }
    };
    let max: usize = flag_value(req, "max")
        .and_then(|s| s.parse().ok())
        .unwrap_or(16_384);
    let truncate = !req.flags.contains_key("full");
    match daemon.inspect_script_source(session_id, page_id, seq) {
        Ok(Some(s)) => {
            let body = if truncate && s.body.len() > max {
                format!("{}...\n", &s.body[..max])
            } else {
                let mut b = s.body.clone();
                if !b.ends_with('\n') {
                    b.push('\n');
                }
                b
            };
            let head = ResponseHead::ok(StateToken::ZERO).encode();
            format!("{head}# url={}\n{body}", s.source_url)
        }
        Ok(None) => format_error(ErrorCode::NotFound, vec![format!("script={seq_arg}")]),
        Err(e) => format_daemon_error(&e),
    }
}

fn render_dom(daemon: &Daemon, session_id: &str, page_id: &str, req: &Request) -> String {
    use std::fmt::Write as _;
    let Some(ref_arg) = req.args.get(2).cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_inspect dom: missing <ref>".into()],
        );
    };
    let r: Ref = match ref_arg.parse() {
        Ok(r) => r,
        Err(_) => {
            return format_error(
                ErrorCode::BadRequest,
                vec![format!("vs_inspect dom: bad ref {ref_arg:?}")],
            );
        }
    };
    let extra: Vec<String> = req
        .flags
        .iter()
        .filter(|(k, _)| k.as_str() == "prop")
        .filter_map(|(_, v)| v.clone())
        .collect();
    match daemon.inspect_dom(session_id, page_id, r, extra) {
        Ok(Some(d)) => {
            let mut body = String::new();
            let _ = writeln!(body, "{}", d.outer_html);
            let _ = writeln!(body, "computed:");
            for (k, v) in &d.computed {
                let _ = writeln!(body, "  {k}: {v}");
            }
            format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
        }
        Ok(None) => format_error(ErrorCode::NotFound, vec![format!("ref={}", r.0)]),
        Err(e) => format_daemon_error(&e),
    }
}

fn render_performance(daemon: &Daemon, session_id: &str, page_id: &str) -> String {
    use std::fmt::Write as _;
    let m = match daemon.inspect_performance(session_id, page_id) {
        Ok(m) => m,
        Err(e) => return format_daemon_error(&e),
    };
    let mut body = String::new();
    let _ = writeln!(
        body,
        "ttfb={:.2} fcp={:.2} lcp={:.2} cls={:.3} fid={:.2}",
        m.ttfb_ms, m.fcp_ms, m.lcp_ms, m.cls, m.fid_ms
    );
    let _ = writeln!(
        body,
        "long_tasks={} total_blocking={:.2}",
        m.long_tasks, m.total_blocking_ms
    );
    let _ = writeln!(
        body,
        "js_heap={:.2} dom_nodes={}",
        m.js_heap_mb, m.dom_nodes
    );
    format!("{}{body}", ResponseHead::ok(StateToken::ZERO).encode())
}
