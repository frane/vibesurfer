//! Building the wire [`Request`] for each subcommand.

use anyhow::{Context as _, Result};
use vs_protocol::Request;

use super::{Command, NetOverrides, NetSub, PendingSub, RecordSub};

impl Command {
    /// Build the wire [`Request`] for this subcommand. Returns `None`
    /// for commands that the CLI handles locally (none yet).
    #[allow(clippy::too_many_lines)]
    pub fn to_request(&self, session_id: Option<&str>) -> Result<Request> {
        Ok(match self {
            Self::SessionOpen {
                policy,
                isolated,
                intercept,
                popups,
                ignore_https_errors,
            } => {
                let mut r = Request::new("vs_session_open");
                if let Some(p) = policy {
                    r = r.flag_value("policy", p.clone());
                }
                if *isolated {
                    r = r.flag("isolated");
                }
                if *intercept {
                    r = r.flag("intercept");
                }
                if *popups {
                    r = r.flag("popups");
                }
                if *ignore_https_errors {
                    r = r.flag("ignore-https-errors");
                }
                r
            }
            Self::SessionClose { all, idle_for } => {
                let mut r = Request::new("vs_session_close");
                if let Some(d) = idle_for {
                    r = r.flag_value("idle-for", d.clone());
                } else if *all {
                    r = r.flag("all");
                } else {
                    r = r.arg(require_session(session_id)?);
                }
                r
            }
            Self::Open { url } => {
                let s = require_session(session_id)?;
                Request::new("vs_open")
                    .arg(url.clone())
                    .flag_value("session", s)
            }
            Self::Goto { page, url, html } => {
                let s = require_session(session_id)?;
                let req = Request::new("vs_goto")
                    .arg(page.clone())
                    .arg(url.clone())
                    .flag_value("session", s);
                match html {
                    Some(path) => {
                        use base64::Engine as _;
                        let bytes = std::fs::read(path).with_context(|| format!("read {path}"))?;
                        req.flag_value(
                            "html",
                            base64::engine::general_purpose::STANDARD.encode(bytes),
                        )
                    }
                    None => req,
                }
            }

            Self::InitScript { page, source } => {
                let s = require_session(session_id)?;
                let js = match source.strip_prefix('@') {
                    Some(path) => std::fs::read_to_string(path)
                        .with_context(|| format!("read init script {path}"))?,
                    None => source.clone(),
                };
                Request::new("vs_init_script")
                    .arg(page.clone())
                    .arg(js)
                    .flag_value("session", s)
            }
            Self::Close { page } => {
                let s = require_session(session_id)?;
                Request::new("vs_close")
                    .arg(page.clone())
                    .flag_value("session", s)
            }
            Self::View { page, full } => {
                let s = require_session(session_id)?;
                let mut r = Request::new("vs_view")
                    .arg(page.clone())
                    .flag_value("session", s);
                if *full {
                    r = r.flag("full");
                }
                r
            }
            Self::Read { page, r } => {
                let s = require_session(session_id)?;
                Request::new("vs_read")
                    .arg(page.clone())
                    .arg(r.to_string())
                    .flag_value("session", s)
            }
            Self::Act {
                page,
                r,
                op,
                value,
                token,
                group,
                mode,
            } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_act")
                    .arg(page.clone())
                    .arg(r.clone())
                    .arg(op.clone());
                if let Some(v) = value {
                    req = req.arg(v.clone());
                }
                req = req
                    .flag_value("session", s)
                    .flag_value("token", token.clone());
                if let Some(g) = group {
                    req = req.flag_value("group", g.clone());
                }
                req = req.flag_value("mode", mode.clone());
                req
            }
            Self::Find { query } => {
                let s = require_session(session_id)?;
                Request::new("vs_find")
                    .arg(query.clone())
                    .flag_value("session", s)
            }
            Self::Wait {
                page,
                cond,
                value,
                timeout,
            } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_wait").arg(page.clone()).arg(cond.clone());
                if let Some(v) = value {
                    req = req.arg(v.clone());
                }
                req.flag_value("session", s)
                    .flag_value("timeout", format!("{timeout}ms"))
            }
            Self::Status => {
                let mut r = Request::new("vs_status");
                if let Some(s) = session_id {
                    r = r.flag_value("session", s.to_string());
                }
                r
            }
            Self::Extract {
                page,
                schema,
                token,
            } => {
                let s = require_session(session_id)?;
                Request::new("vs_extract")
                    .arg(page.clone())
                    .arg(schema.clone())
                    .flag_value("session", s)
                    .flag_value("token", token.clone())
            }
            Self::Mark {
                page,
                r,
                name,
                token,
            } => {
                let s = require_session(session_id)?;
                Request::new("vs_mark")
                    .arg(page.clone())
                    .arg(r.to_string())
                    .arg(name.clone())
                    .flag_value("session", s)
                    .flag_value("token", token.clone())
            }
            Self::Annotate { target, key, value } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_annotate")
                    .arg(target.clone())
                    .arg(key.clone());
                if let Some(v) = value {
                    req = req.arg(v.clone());
                }
                req.flag_value("session", s)
            }
            Self::Log {
                page,
                group,
                since,
                limit,
            } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_log").flag_value("session", s);
                if let Some(p) = page {
                    req = req.flag_value("page", p.clone());
                }
                if let Some(g) = group {
                    req = req.flag_value("group", g.clone());
                }
                if let Some(t) = since {
                    req = req.flag_value("since", t.to_string());
                }
                if let Some(l) = limit {
                    req = req.flag_value("limit", l.to_string());
                }
                req
            }
            Self::Skill { sub, name } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_skill").flag_value("session", s);
                let sub = sub.as_deref().unwrap_or("list");
                req = req.arg(sub.to_string());
                if let Some(n) = name {
                    req = req.arg(n.clone());
                }
                req
            }
            Self::Capture {
                clean: _,
                page,
                r,
                full_page,
                base64: _,
            } => {
                // `clean` is handled locally in dispatch.rs before any
                // wire call, so by here we're taking a screenshot and a
                // page id is required. `base64` is a CLI-side
                // post-process — the daemon still returns the on-disk
                // path; dispatch.rs reads the PNG and base64-encodes it
                // before printing.
                let page = page.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("vs capture: missing page id (or use `vs capture clean`)")
                })?;
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_capture")
                    .arg(page.clone())
                    .flag_value("session", s);
                if let Some(rr) = r {
                    req = req.arg(rr.to_string());
                }
                if *full_page {
                    req = req.flag("full-page");
                }
                req
            }
            Self::Download {
                page,
                url,
                dest,
                list,
                id,
                timeout_ms,
            } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_download")
                    .arg(page.clone())
                    .flag_value("session", s);
                if *list {
                    return Ok(req.flag("list"));
                }
                if let Some(u) = url {
                    req = req.arg(u.clone());
                }
                if let Some(d) = dest {
                    req = req.flag_value("dest", d.clone());
                }
                if let Some(i) = id {
                    req = req.flag_value("id", i.to_string());
                }
                req.flag_value("timeout-ms", timeout_ms.to_string())
            }
            Self::Viewport {
                page,
                spec,
                dpr,
                zoom,
            } => {
                let s = require_session(session_id)?;
                let mut r = Request::new("vs_viewport")
                    .arg(page.clone())
                    .arg(spec.clone())
                    .flag_value("session", s)
                    .flag_value("dpr", dpr.to_string());
                if let Some(z) = zoom {
                    r = r.flag_value("zoom", z.to_string());
                }
                r
            }
            Self::Layout { page, refs } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_layout").arg(page.clone());
                for r in refs {
                    req = req.arg(r.to_string());
                }
                req.flag_value("session", s)
            }
            Self::Auth { sub, rest } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_auth")
                    .arg(sub.clone())
                    .flag_value("session", s);
                for r in rest {
                    req = req.arg(r.clone());
                }
                req
            }
            Self::Inspect {
                page,
                kind,
                rest,
                since,
                level,
                status,
                max,
                full,
                unsafe_log,
                frame,
            } => {
                let s = require_session(session_id)?;
                let kind_long = normalize_inspect_kind(kind);
                let mut req = Request::new("vs_inspect")
                    .arg(kind_long.to_string())
                    .arg(page.clone());
                for r in rest {
                    req = req.arg(r.clone());
                }
                req = req.flag_value("session", s);
                if let Some(v) = since {
                    req = req.flag_value("since", v.clone());
                }
                if let Some(v) = level {
                    req = req.flag_value("level", v.clone());
                }
                if let Some(v) = status {
                    req = req.flag_value("status", v.clone());
                }
                if let Some(v) = max {
                    req = req.flag_value("max", v.clone());
                }
                if *full {
                    req = req.flag("full");
                }
                if *unsafe_log {
                    req = req.flag("unsafe-log");
                }
                if let Some(v) = frame {
                    req = req.flag_value("frame", v.clone());
                }
                req
            }
            Self::MoveTo { page, x, y, mode } => {
                let s = require_session(session_id)?;
                Request::new("vs_move_to")
                    .arg(page.clone())
                    .arg(x.to_string())
                    .arg(y.to_string())
                    .flag_value("session", s)
                    .flag_value("mode", mode.clone())
            }
            Self::ClickAt {
                page,
                x,
                y,
                token,
                mode,
                button,
                count,
                modifiers,
            } => {
                let s = require_session(session_id)?;
                let mut r = Request::new("vs_click_at")
                    .arg(page.clone())
                    .arg(x.to_string())
                    .arg(y.to_string());
                if let Some(b) = button {
                    r = r.flag_value("button", b.clone());
                }
                if let Some(c) = count {
                    r = r.flag_value("count", c.to_string());
                }
                if let Some(m) = modifiers {
                    r = r.flag_value("modifiers", m.clone());
                }
                r.flag_value("session", s)
                    .flag_value("token", token.clone())
                    .flag_value("mode", mode.clone())
            }
            Self::HoverAt { page, x, y, mode } => {
                let s = require_session(session_id)?;
                Request::new("vs_hover_at")
                    .arg(page.clone())
                    .arg(x.to_string())
                    .arg(y.to_string())
                    .flag_value("session", s)
                    .flag_value("mode", mode.clone())
            }
            Self::DebugCrash { page } => {
                let s = require_session(session_id)?;
                Request::new("vs_debug_crash")
                    .arg(page.clone())
                    .flag_value("session", s)
            }
            Self::Cookies { page, op, data } => {
                use base64::Engine as _;
                let s = require_session(session_id)?;
                let mut r = Request::new("vs_cookies").arg(page.clone()).arg(op.clone());
                if let Some(j) = data {
                    r = r.arg(base64::engine::general_purpose::STANDARD.encode(j.as_bytes()));
                }
                r.flag_value("session", s)
            }
            Self::Drop {
                page,
                x,
                y,
                files,
                data,
            } => {
                use base64::Engine as _;
                let s = require_session(session_id)?;
                let mut items = Vec::new();
                for d in data {
                    let (mime, value) = d
                        .split_once('=')
                        .with_context(|| format!("--data {d:?}: want MIME=VALUE"))?;
                    items.push((mime.to_string(), value.to_string()));
                }
                let files: Vec<String> = files
                    .iter()
                    .map(|f| {
                        std::path::absolute(f)
                            .map_or_else(|_| f.clone(), |p| p.to_string_lossy().into_owned())
                    })
                    .collect();
                let payload = serde_json::json!({ "files": files, "data": items });
                Request::new("vs_drop")
                    .arg(page.clone())
                    .arg(x.to_string())
                    .arg(y.to_string())
                    .arg(base64::engine::general_purpose::STANDARD.encode(payload.to_string()))
                    .flag_value("session", s)
            }
            Self::Wheel {
                page,
                x,
                y,
                dx,
                dy,
                mode,
            } => {
                let s = require_session(session_id)?;
                Request::new("vs_wheel")
                    .arg(page.clone())
                    .arg(x.to_string())
                    .arg(y.to_string())
                    .arg(dx.to_string())
                    .arg(dy.to_string())
                    .flag_value("session", s)
                    .flag_value("mode", mode.clone())
            }
            Self::Type {
                page,
                text,
                secret,
                mode,
            } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_type")
                    .arg(page.clone())
                    .arg(text.clone())
                    .flag_value("session", s)
                    .flag_value("mode", mode.clone());
                if *secret {
                    req = req.flag("secret");
                }
                req
            }
            Self::Drag {
                page,
                x1,
                y1,
                x2,
                y2,
                token,
                mode,
            } => {
                let s = require_session(session_id)?;
                Request::new("vs_drag")
                    .arg(page.clone())
                    .arg(x1.to_string())
                    .arg(y1.to_string())
                    .arg(x2.to_string())
                    .arg(y2.to_string())
                    .flag_value("session", s)
                    .flag_value("token", token.clone())
                    .flag_value("mode", mode.clone())
            }
            Self::PromptInput { .. }
            | Self::PromptConfirm { .. }
            | Self::PromptForm { .. }
            | Self::PromptScan { .. } => {
                anyhow::bail!("vs_prompt_* is local; route via main, not the wire dispatcher");
            }
            Self::PromptInputQueue {
                page,
                r,
                message,
                secret,
                token,
                group,
                timeout_ms,
            } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_prompt_input_queue")
                    .arg(page.clone())
                    .arg(r.to_string())
                    .arg(message.clone())
                    .flag_value("session", s)
                    .flag_value("token", token.clone())
                    .flag_value("timeout-ms", timeout_ms.to_string());
                if *secret {
                    req = req.flag("secret");
                }
                if let Some(g) = group {
                    req = req.flag_value("group", g.clone());
                }
                req
            }
            Self::Watch { page, .. } => {
                let s = require_session(session_id)?;
                Request::new("vs_watch")
                    .arg(page.clone())
                    .flag_value("session", s)
            }
            Self::Frame { page } => Request::new("vs_frame").arg(page.clone()),
            Self::PromptFormWait { form, timeout_ms } => {
                let s = require_session(session_id)?;
                Request::new("vs_prompt_form_wait")
                    .arg(form.clone())
                    .flag_value("session", s)
                    .flag_value("timeout-ms", timeout_ms.to_string())
            }
            Self::Pending { sub } => match sub {
                PendingSub::List => Request::new("vs_pending_list"),
                PendingSub::Fulfill { id } => {
                    // CLI side reads value from tty before sending the
                    // wire request — handled in dispatch.rs. Here we
                    // just stub a placeholder; dispatch overrides the
                    // value arg with what the user typed.
                    let id_v = id.clone().unwrap_or_default();
                    Request::new("vs_pending_fulfill")
                        .arg(id_v)
                        .arg(String::new())
                }
                PendingSub::Cancel { id } => Request::new("vs_pending_cancel").arg(id.clone()),
                PendingSub::Url => Request::new("vs_pending_url"),
            },
            Self::Net { sub } => {
                let s = require_session(session_id)?;
                let r = match sub {
                    NetSub::Poll { wait_ms } => {
                        let r = Request::new("vs_net_poll");
                        if *wait_ms > 0 {
                            r.flag_value("wait-ms", wait_ms.to_string())
                        } else {
                            r
                        }
                    }
                    NetSub::Route { state } => Request::new("vs_net_route").arg(state.clone()),
                    NetSub::Continue { id, o } => {
                        o.apply(Request::new("vs_net_continue").arg(id.to_string()))
                    }
                    NetSub::Fulfill {
                        id,
                        status,
                        headers,
                        body_b64,
                    } => {
                        let mut r = Request::new("vs_net_fulfill")
                            .arg(id.to_string())
                            .flag_value("status", status.to_string());
                        r = net_headers(r, headers);
                        if let Some(b) = body_b64 {
                            r = r.flag_value("body-b64", b.clone());
                        }
                        r
                    }
                    NetSub::Abort { id, error } => {
                        let r = Request::new("vs_net_abort").arg(id.to_string());
                        match error {
                            Some(e) => r.flag_value("error", e.clone()),
                            None => r,
                        }
                    }
                    NetSub::Fetch { id, o } => {
                        o.apply(Request::new("vs_net_fetch").arg(id.to_string()))
                    }
                    NetSub::Body { id } => Request::new("vs_net_body").arg(id.to_string()),
                    NetSub::Dialog { id, action, text } => {
                        let r = Request::new("vs_net_dialog")
                            .arg(id.to_string())
                            .arg(action.clone());
                        match text {
                            Some(t) => r.flag_value("text", t.clone()),
                            None => r,
                        }
                    }
                };
                r.flag_value("session", s)
            }
            Self::Record { sub } => {
                let s = require_session(session_id)?;
                match sub {
                    RecordSub::Start {
                        page,
                        fps,
                        width,
                        retina,
                        format,
                    } => {
                        let mut r = Request::new("vs_record")
                            .arg("start")
                            .arg(page.clone())
                            .flag_value("session", s)
                            .flag_value("fps", fps.to_string());
                        if let Some(w) = width {
                            r = r.flag_value("width", w.to_string());
                        }
                        if *retina {
                            r = r.flag("retina");
                        }
                        if let Some(f) = format {
                            r = r.flag_value("format", f.clone());
                        }
                        r
                    }
                    RecordSub::Stop { page } => Request::new("vs_record")
                        .arg("stop")
                        .arg(page.clone())
                        .flag_value("session", s),
                }
            }
            Self::Serve { .. } => {
                anyhow::bail!("vs_serve is local; route via main, not the wire dispatcher");
            }
            Self::Socket => {
                anyhow::bail!("vs socket is local; route via main, not the wire dispatcher");
            }
            Self::Flow { .. } => {
                anyhow::bail!("vs_flow is local; route via main, not the wire dispatcher");
            }
            Self::Mcp => {
                anyhow::bail!("vs_mcp is local; route via main, not the wire dispatcher");
            }
        })
    }

    /// True if this subcommand requires an active session.
    #[must_use]
    pub fn needs_session(&self) -> bool {
        !matches!(
            self,
            Self::SessionOpen { .. }
                | Self::Status
                | Self::Serve { .. }
                | Self::Socket
                | Self::Mcp
                | Self::Flow { .. }
                | Self::Pending { .. }
                | Self::Frame { .. }
        )
    }
}

fn require_session(session: Option<&str>) -> Result<String> {
    session
        .map(str::to_string)
        .context("no active session — run `vs session-open` or pass `--session=<id>`")
}

/// Map short-form inspect kind aliases to their long form. Unknown
/// inputs pass through unchanged so the wire-side parser can reject
/// or accept them — the CLI does not gatekeep here. The two-letter
/// short forms are unambiguous within the inspect subcommand set.
fn normalize_inspect_kind(kind: &str) -> &str {
    match kind {
        "co" => "console",
        "n" => "network",
        "req" => "request",
        "e" => "eval",
        "s" => "storage",
        "scr" => "scripts",
        "src" => "script",
        "res" => "response",
        "d" => "dom",
        "p" => "performance",
        "ce" => "cookie-events",
        other => other,
    }
}

impl NetOverrides {
    fn apply(&self, mut r: Request) -> Request {
        if let Some(u) = &self.url {
            r = r.flag_value("url", u.clone());
        }
        if let Some(m) = &self.method {
            r = r.flag_value("method", m.clone());
        }
        r = net_headers(r, &self.headers);
        if let Some(b) = &self.body_b64 {
            r = r.flag_value("body-b64", b.clone());
        }
        r
    }
}

/// Repeated `--header` values travel as one newline-joined `--headers`.
fn net_headers(r: Request, headers: &[String]) -> Request {
    if headers.is_empty() {
        r
    } else {
        r.flag_value("headers", headers.join("\n"))
    }
}
