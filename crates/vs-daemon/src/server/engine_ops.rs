//! Wire handlers for engine-backed primitives: skill, capture,
//! download, viewport, cursor and typing input, cookies, drop, layout, auth.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use vs_engine_webkit::{CaptureScope, DownloadSource, Viewport};
use vs_protocol::{ErrorCode, Ref, Request, ResponseHead, StateToken};

use super::helpers::{flag_value, format_daemon_error, format_error, require_session};
use crate::daemon::{
    AuthListResponse, AuthLoadResponse, AuthSaveResponse, CaptureResponse, Daemon,
    DownloadListResponse, DownloadResponse, LayoutResponse, SkillListResponse, SkillShowResponse,
    ViewportResponse,
};
use crate::page_state::ViewForm;

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

/// Flatten page-supplied text so it cannot break a tab-separated body
/// row. Filenames and MIME types come from the page; a tab or newline
/// in one would shift every column after it.
fn one_line(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

/// Default budget for a download: enough for a slow multi-megabyte PDF
/// behind an authenticated session, short enough that a wedged fetch
/// doesn't hold an agent's tool call open forever. Override with
/// `--timeout-ms`.
const DOWNLOAD_BUDGET_MS: u64 = 30_000;

/// `vs_download` — write a file out of a page to disk.
///
/// Args: `<page> [url]`. With a URL, the page fetches it with its own
/// cookies. Without one, the newest download the page *tried* to
/// perform is drained from the capture shim (`--id=N` picks a specific
/// buffered entry, `--list` just enumerates them).
///
/// Body: `path`/`size`/`mime`/`url` rows, or one `dl` row per entry for
/// `--list`. Bytes never cross the wire — only the path does.
pub(super) fn handle_download(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_download: missing page id".into()],
        );
    };

    if req.flags.contains_key("list") {
        return match daemon.download_list(&session_id, &page_id) {
            Ok(DownloadListResponse { entries, token }) => {
                let mut body = String::new();
                for e in entries {
                    use std::fmt::Write as _;
                    // The last column is the URL, or `err=<why>` when
                    // the capture failed — a failed download stays
                    // listed so the agent sees the reason instead of an
                    // entry that silently never appeared.
                    let last = match &e.error {
                        Some(msg) => format!("err={}", one_line(msg)),
                        None => one_line(&e.url),
                    };
                    let _ = writeln!(
                        body,
                        "dl\t{}\t{}\t{}\t{}\t{}\t{last}",
                        e.id,
                        e.size,
                        if e.done { "ready" } else { "pending" },
                        one_line(&e.mime),
                        one_line(&e.filename),
                    );
                }
                format!("{}{body}", ResponseHead::ok(token).encode())
            }
            Err(e) => format_daemon_error(&e),
        };
    }

    let source = match req.args.get(1) {
        Some(url) => DownloadSource::Url(url.clone()),
        None => DownloadSource::Captured {
            id: flag_value(req, "id").and_then(|s| s.parse::<u64>().ok()),
        },
    };
    let budget = std::time::Duration::from_millis(
        flag_value(req, "timeout-ms")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DOWNLOAD_BUDGET_MS),
    );
    let dest = flag_value(req, "dest");
    match daemon.download(&session_id, &page_id, source, dest.as_deref(), budget) {
        Ok(DownloadResponse {
            path,
            size,
            mime,
            url,
            token,
        }) => format!(
            "{}path\t{}\nsize\t{size}\nmime\t{}\nurl\t{}\n",
            ResponseHead::ok(token).encode(),
            path.display(),
            one_line(&mime),
            one_line(&url),
        ),
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
    if let Some(z) = flag_value(req, "zoom") {
        let Some(zoom) = z.parse::<f64>().ok().filter(|z| *z > 0.0 && *z <= 10.0) else {
            return format_error(
                ErrorCode::BadRequest,
                vec![format!("vs_viewport: bad --zoom={z:?}")],
            );
        };
        if let Err(e) = daemon.zoom(&session_id, &page_id, zoom) {
            return format_daemon_error(&e);
        }
        if spec == "keep" {
            return ResponseHead::ok(StateToken::ZERO).encode();
        }
    }
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

/// `vs_type` — trusted keyboard typing into the focused element.
/// Args: `<page> <text>`. Flags: `--secret` (redact in audit),
/// `--mode`. The caller places the caret first (e.g. vs_click_at).
pub(super) fn handle_type(daemon: &Daemon, req: &Request) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_type: missing page id".into()],
        );
    };
    let Some(text) = req.args.get(1).cloned() else {
        return format_error(ErrorCode::BadRequest, vec!["vs_type: missing text".into()]);
    };
    let secret = req.flags.contains_key("secret");
    let mode_str = flag_value(req, "mode").unwrap_or_else(|| "human".into());
    let Some(mode) = vs_engine_webkit::engine::InputMode::parse(&mode_str) else {
        return format_error(
            ErrorCode::BadRequest,
            vec![format!(
                "vs_type: bad --mode={mode_str:?} (human|careful|robotic)"
            )],
        );
    };
    match daemon.type_text(&session_id, &page_id, &text, secret, mode) {
        Ok(token) => ResponseHead::ok(token).encode(),
        Err(e) => format_daemon_error(&e),
    }
}
/// `--button left|right|middle`, `--count 1..3`, `--modifiers
/// shift,ctrl,alt,meta` on `vs_click_at`; `None` for a plain click.
fn click_opts(req: &Request) -> Result<Option<vs_engine_webkit::ClickOpts>, String> {
    use vs_engine_webkit::{ClickOpts, Modifiers, MouseButton};
    let button = match flag_value(req, "button").as_deref() {
        None | Some("left") => MouseButton::Left,
        Some("right") => MouseButton::Right,
        Some("middle") => MouseButton::Middle,
        Some(b) => {
            return Err(format!(
                "vs_click_at: bad --button={b:?} (left|right|middle)"
            ))
        }
    };
    let count = match flag_value(req, "count") {
        None => 1,
        Some(c) => c
            .parse::<u8>()
            .ok()
            .filter(|n| (1..=3).contains(n))
            .ok_or_else(|| format!("vs_click_at: bad --count={c:?} (1..3)"))?,
    };
    let modifiers = match flag_value(req, "modifiers") {
        None => Modifiers::default(),
        Some(m) => Modifiers::parse(&m)
            .ok_or_else(|| format!("vs_click_at: bad --modifiers={m:?} (shift,ctrl,alt,meta)"))?,
    };
    if button == MouseButton::Left && count == 1 && !modifiers.any() {
        return Ok(None);
    }
    Ok(Some(ClickOpts {
        button,
        count,
        modifiers,
    }))
}

pub(super) fn handle_cursor(daemon: &Daemon, req: &Request, kind: &str) -> String {
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first().cloned() else {
        return format_error(
            ErrorCode::BadRequest,
            vec![format!("vs_{kind}: missing page id")],
        );
    };
    let parse_f64 = |idx: usize, name: &str| -> Result<f64, String> {
        req.args
            .get(idx)
            .ok_or_else(|| format!("vs_{kind}: missing {name}"))
            .and_then(|s| {
                s.parse::<f64>()
                    .map_err(|_| format!("vs_{kind}: bad {name}={s:?}"))
            })
    };
    let mode_str = flag_value(req, "mode").unwrap_or_else(|| "human".into());
    let Some(mode) = vs_engine_webkit::engine::InputMode::parse(&mode_str) else {
        return format_error(
            ErrorCode::BadRequest,
            vec![format!(
                "vs_{kind}: bad --mode={mode_str:?} (expected human|careful|robotic)"
            )],
        );
    };
    let op_result = match kind {
        "move_to" => parse_f64(1, "x").and_then(|x| {
            parse_f64(2, "y").map(|y| vs_engine_webkit::engine::CursorOp::MoveTo { x, y })
        }),
        "click_at" => parse_f64(1, "x").and_then(|x| {
            parse_f64(2, "y").and_then(|y| match click_opts(req) {
                Ok(None) => Ok(vs_engine_webkit::engine::CursorOp::ClickAt { x, y }),
                Ok(Some(opts)) => Ok(vs_engine_webkit::engine::CursorOp::Click { x, y, opts }),
                Err(e) => Err(e),
            })
        }),
        "hover_at" => parse_f64(1, "x").and_then(|x| {
            parse_f64(2, "y").map(|y| vs_engine_webkit::engine::CursorOp::HoverAt { x, y })
        }),
        "drag" => parse_f64(1, "x1").and_then(|x1| {
            parse_f64(2, "y1").and_then(|y1| {
                parse_f64(3, "x2").and_then(|x2| {
                    parse_f64(4, "y2").map(|y2| vs_engine_webkit::engine::CursorOp::Drag {
                        x1,
                        y1,
                        x2,
                        y2,
                    })
                })
            })
        }),
        "wheel" => parse_f64(1, "x").and_then(|x| {
            parse_f64(2, "y").and_then(|y| {
                parse_f64(3, "dx").and_then(|dx| {
                    parse_f64(4, "dy").map(|dy| vs_engine_webkit::engine::CursorOp::Wheel {
                        x,
                        y,
                        dx,
                        dy,
                    })
                })
            })
        }),
        _ => Err(format!("vs_{kind}: unknown kind")),
    };
    let op = match op_result {
        Ok(o) => o,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    // Token check for click_at / drag (mutating ops).
    if matches!(kind, "click_at" | "drag") {
        let Some(_token) = flag_value(req, "token") else {
            return format_error(
                ErrorCode::BadRequest,
                vec![format!("vs_{kind}: missing --token=<state token>")],
            );
        };
    }
    match daemon.cursor_op(&session_id, &page_id, op, mode) {
        Ok((token, form)) => {
            let head = vs_protocol::ResponseHead::ok(token).encode();
            let body = match form {
                ViewForm::Full(tree) => tree.encode(),
                ViewForm::Delta(ops) => vs_protocol::delta::encode(&ops),
                ViewForm::NoChange => String::new(),
            };
            format!("{head}{body}")
        }
        Err(e) => format_daemon_error(&e),
    }
}
/// `vs_debug_crash <page>`: kill the page's web content process, for
/// crash-handling tests. Only with `VS_ALLOW_CRASH=1` in the daemon's
/// environment.
pub(super) fn handle_debug_crash(daemon: &Daemon, req: &Request) -> String {
    if std::env::var("VS_ALLOW_CRASH").as_deref() != Ok("1") {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_debug_crash: needs VS_ALLOW_CRASH=1".into()],
        );
    }
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let Some(page_id) = req.args.first() else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_debug_crash: missing page id".into()],
        );
    };
    match daemon.debug_crash(&session_id, page_id) {
        Ok(()) => ResponseHead::ok(StateToken::ZERO).encode(),
        Err(e) => format_daemon_error(&e),
    }
}

/// `vs_cookies <page> list|clear|set <base64 JSON array>`: the jar of
/// the page's data store, as one JSON array line, after the change.
pub(super) fn handle_cookies(daemon: &Daemon, req: &Request) -> String {
    use base64::Engine as _;
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let (Some(page_id), Some(sub)) = (req.args.first(), req.args.get(1)) else {
        return format_error(
            ErrorCode::BadRequest,
            vec!["vs_cookies: usage <page> list|set <b64 json>|clear".into()],
        );
    };
    let op = match sub.as_str() {
        "list" => vs_engine_webkit::CookieOp::List,
        "clear" => vs_engine_webkit::CookieOp::Clear,
        "set" => {
            let parsed = req
                .args
                .get(2)
                .ok_or_else(|| "vs_cookies set: missing cookies".to_string())
                .and_then(|b| {
                    base64::engine::general_purpose::STANDARD
                        .decode(b)
                        .map_err(|e| format!("vs_cookies set: bad base64: {e}"))
                })
                .and_then(|bytes| {
                    serde_json::from_slice::<Vec<vs_engine_webkit::CookieData>>(&bytes)
                        .map_err(|e| format!("vs_cookies set: bad JSON: {e}"))
                });
            match parsed {
                Ok(list) => vs_engine_webkit::CookieOp::Set(list),
                Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
            }
        }
        other => {
            return format_error(
                ErrorCode::BadRequest,
                vec![format!("vs_cookies: unknown {other:?} (list|set|clear)")],
            )
        }
    };
    match daemon.cookies(&session_id, page_id, op) {
        Ok(jar) => format!(
            "{}{}\n",
            ResponseHead::ok(StateToken::ZERO).encode(),
            serde_json::to_string(&jar).unwrap_or_else(|_| "[]".into())
        ),
        Err(e) => format_daemon_error(&e),
    }
}

/// `vs_drop <page> <x> <y> <b64 JSON {"files":[path],"data":{mime:value}}>`.
pub(super) fn handle_drop(daemon: &Daemon, req: &Request) -> String {
    use base64::Engine as _;
    let session_id = match require_session(req) {
        Ok(s) => s,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let usage = || {
        format_error(
            ErrorCode::BadRequest,
            vec!["vs_drop: usage <page> <x> <y> <b64 JSON {files, data}>".into()],
        )
    };
    let (Some(page_id), Some(x), Some(y), Some(b64)) = (
        req.args.first(),
        req.args.get(1),
        req.args.get(2),
        req.args.get(3),
    ) else {
        return usage();
    };
    let (Ok(x), Ok(y)) = (x.parse::<f64>(), y.parse::<f64>()) else {
        return usage();
    };
    let payload = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("vs_drop: bad base64: {e}"))
        .and_then(|b| {
            serde_json::from_slice::<serde_json::Value>(&b)
                .map_err(|e| format!("vs_drop: bad JSON: {e}"))
        });
    let payload = match payload {
        Ok(p) => p,
        Err(msg) => return format_error(ErrorCode::BadRequest, vec![msg]),
    };
    let strings = |v: &serde_json::Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let files: Vec<std::path::PathBuf> = strings(&payload["files"])
        .into_iter()
        .map(Into::into)
        .collect();
    let items: Vec<(String, String)> = payload["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|pair| {
                    let p = strings(pair);
                    (p.len() == 2).then(|| (p[0].clone(), p[1].clone()))
                })
                .collect()
        })
        .unwrap_or_default();
    for f in &files {
        if !f.is_file() {
            return format_error(
                ErrorCode::BadRequest,
                vec![format!("vs_drop: no such file {}", f.display())],
            );
        }
    }
    let data = vs_engine_webkit::DropData { files, items };
    match daemon.drop_data(&session_id, page_id, x, y, data) {
        Ok(accepted) => format!(
            "{}{}\n",
            ResponseHead::ok(StateToken::ZERO).encode(),
            if accepted { "accepted" } else { "refused" }
        ),
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

#[allow(clippy::too_many_lines)]
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
        "webauthn" => {
            let Some(page_id) = req.args.get(1).cloned() else {
                return format_error(
                    ErrorCode::BadRequest,
                    vec!["vs_auth webauthn: missing page id".into()],
                );
            };
            match daemon.enable_webauthn(&session_id, &page_id) {
                Ok(AuthSaveResponse { name, .. }) => {
                    format!("{}{name}\n", ResponseHead::ok(StateToken::ZERO).encode())
                }
                Err(e) => format_daemon_error(&e),
            }
        }
        "import" => {
            let Some(name) = req.args.get(1).cloned() else {
                return format_error(
                    ErrorCode::BadRequest,
                    vec!["vs_auth import: missing name".into()],
                );
            };
            let Some(b64) = req.args.get(2).cloned() else {
                return format_error(
                    ErrorCode::BadRequest,
                    vec!["vs_auth import: missing base64 blob".into()],
                );
            };
            let blob = match STANDARD.decode(b64.as_bytes()) {
                Ok(b) => b,
                Err(e) => {
                    return format_error(
                        ErrorCode::BadRequest,
                        vec![format!("vs_auth import: bad base64: {e}")],
                    )
                }
            };
            match daemon.auth_import(&session_id, &name, &blob) {
                Ok(AuthSaveResponse { name, .. }) => {
                    format!("{}{name}\n", ResponseHead::ok(StateToken::ZERO).encode())
                }
                Err(e) => format_daemon_error(&e),
            }
        }
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
                Ok(AuthSaveResponse { name, warnings }) => {
                    let mut head = ResponseHead::ok(StateToken::ZERO);
                    head.warnings = warnings;
                    format!("{}{name}\n", head.encode())
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
