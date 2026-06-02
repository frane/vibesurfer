//! `clap`-derived command tree and dispatch.
//!
//! Each primitive is one subcommand. [`run`] builds a [`Request`] from
//! the subcommand, calls [`Client::call`](crate::client::Client::call),
//! and returns the response; [`render`] formats it for stdout.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use vs_protocol::Request;

/// Top-level CLI.
#[derive(Debug, Parser)]
#[command(
    name = "vs",
    version,
    about = "vibesurfer — agent-native browser CLI",
    long_about = "vibesurfer client and daemon. `vs serve` runs the daemon; everything else sends a request to it over a Unix socket."
)]
pub struct Cli {
    /// Override the active session id (otherwise read from
    /// `$VIBESURFER_HOME/active-session`).
    #[arg(long, short = 'S', global = true)]
    pub session: Option<String>,

    /// Override the daemon socket path. Tests pass an explicit path;
    /// in production this defaults to `$HOME/.vibesurfer/daemon.sock`.
    #[arg(long, global = true)]
    pub socket: Option<PathBuf>,

    /// Override the vibesurfer home directory. Useful for tests.
    #[arg(long, global = true)]
    pub home: Option<PathBuf>,

    /// Skip the daemon auto-spawn step. Tests start the daemon
    /// themselves and connect via `--socket`.
    #[arg(long, global = true)]
    pub no_spawn: bool,

    /// Emit the response as JSON for human inspection. The default is
    /// the line-oriented wire form that agents consume.
    #[arg(long, short = 'j', global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// The subcommand vocabulary. One variant per primitive.
///
/// Each variant has a `visible_alias` short form for token economy in
/// agent contexts (mirrors agented's `s` / `i` / `d` / `w` / `br`
/// pattern). Long forms remain for human readers.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// 1. Open a new session.
    #[command(visible_alias = "so")]
    SessionOpen {
        #[arg(long)]
        policy: Option<String>,
    },
    /// 2. Close the active session.
    #[command(visible_alias = "sc")]
    SessionClose,
    /// 3. Open a page navigated to URL.
    #[command(visible_alias = "o")]
    Open { url: String },
    /// 4. Close a page.
    #[command(visible_alias = "c")]
    Close { page: String },
    /// 5. View a page (delta by default; `--full` re-baselines).
    #[command(visible_alias = "v")]
    View {
        page: String,
        #[arg(long, short = 'F')]
        full: bool,
    },
    /// 6. Read the full text of a ref.
    #[command(visible_alias = "r")]
    Read {
        page: String,
        #[arg(value_name = "REF")]
        r: u32,
    },
    /// 7. Perform an action on a ref.
    #[command(visible_alias = "a")]
    Act {
        page: String,
        #[arg(value_name = "REF")]
        r: u32,
        op: String,
        value: Option<String>,
        #[arg(long)]
        token: String,
        #[arg(long)]
        group: Option<String>,
    },
    /// 8. Search across pages in the session.
    #[command(visible_alias = "f")]
    Find { query: String },
    /// 9. Wait for a condition on a page.
    #[command(visible_alias = "w")]
    Wait {
        page: String,
        cond: String,
        value: Option<String>,
        #[arg(long, default_value_t = 5000)]
        timeout: u64,
    },
    /// 13. Status summary for the active session (or all sessions).
    #[command(visible_alias = "st")]
    Status,
    /// 10. Extract structured data using a known schema.
    #[command(visible_alias = "x")]
    Extract {
        page: String,
        schema: String,
        #[arg(long)]
        token: String,
    },
    /// 11. Persist a ref as a named anchor.
    #[command(visible_alias = "m")]
    Mark {
        page: String,
        #[arg(value_name = "REF")]
        r: u32,
        name: String,
        #[arg(long)]
        token: String,
    },
    /// 12. Attach a (key, value) annotation to a target (one of
    ///     `ref:N`, `mark:NAME`, or `page`).
    #[command(visible_alias = "an")]
    Annotate {
        target: String,
        key: String,
        value: Option<String>,
    },
    /// 14. Slice the audit log.
    #[command(visible_alias = "l")]
    Log {
        #[arg(long, short = 'P')]
        page: Option<String>,
        #[arg(long)]
        group: Option<String>,
        #[arg(long, short = 's')]
        since: Option<i64>,
        #[arg(long, short = 'n')]
        limit: Option<i64>,
    },
    /// 15. Skill management. Subcommand: `list` or `show <name>`.
    ///     (M6 adds `<name> [args]` execution.)
    #[command(visible_alias = "sk")]
    Skill {
        sub: Option<String>,
        name: Option<String>,
    },
    /// 16. Capture a screenshot. Defaults to viewport scope; pass a
    ///     ref to capture an element, or `--full-page`.
    #[command(visible_alias = "cap")]
    Capture {
        page: String,
        #[arg(value_name = "REF")]
        r: Option<u32>,
        #[arg(long)]
        full_page: bool,
        /// Emit the PNG bytes as base64 on the response body (instead
        /// of just a path on disk). Lets MCP-driven agents see the
        /// screenshot inline; the on-disk PNG is still written.
        #[arg(long, alias = "b64")]
        base64: bool,
    },
    /// 17. Set the viewport. `spec` is a preset (e.g. `mobile`,
    ///     `desktop`) or `WxH` (e.g. `1280x720`).
    #[command(visible_alias = "vp")]
    Viewport {
        page: String,
        spec: String,
        #[arg(long, default_value_t = 2)]
        dpr: u32,
    },
    /// 18. Compute layout boxes for one or more refs.
    #[command(visible_alias = "lay")]
    Layout {
        page: String,
        #[arg(value_name = "REF", required = true)]
        refs: Vec<u32>,
    },
    /// 19. Auth blob management. Subcommand: `save <page> <name>`,
    ///     `load <page> <name>`, `list`, or `clear <name>`.
    #[command(visible_alias = "au")]
    Auth {
        sub: String,
        #[arg(num_args = 0..=2)]
        rest: Vec<String>,
    },
    /// 20. Inspect engine state — console, network, request detail,
    ///     storage, scripts, dom, performance. The first positional is
    ///     the page id; the second is the kind. Trailing positionals
    ///     are kind-specific (e.g. `request <seq>`, `eval <expr>`,
    ///     `storage <scope>`, `script <seq>`).
    #[command(visible_alias = "i")]
    Inspect {
        page: String,
        kind: String,
        #[arg(num_args = 0..=3)]
        rest: Vec<String>,
        #[arg(long, short = 's')]
        since: Option<String>,
        #[arg(long)]
        level: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        max: Option<String>,
        #[arg(long, short = 'F')]
        full: bool,
        #[arg(long = "unsafe-log")]
        unsafe_log: bool,
    },
    /// Move the cursor to `(x, y)` along a humanized Bezier path.
    /// Native trusted-event dispatch on macOS; ENGINE_UNSUPPORTED
    /// elsewhere until M7 wires GDK / CDP input.
    #[command(visible_alias = "mt")]
    MoveTo {
        page: String,
        x: f64,
        y: f64,
        #[arg(long, short = 'M', default_value = "human")]
        mode: String,
    },
    /// Click at `(x, y)`. Trusted on macOS (`isTrusted = true`).
    #[command(visible_alias = "ca")]
    ClickAt {
        page: String,
        x: f64,
        y: f64,
        #[arg(long)]
        token: String,
        #[arg(long, short = 'M', default_value = "human")]
        mode: String,
    },
    /// Hover at `(x, y)`.
    #[command(visible_alias = "ha")]
    HoverAt {
        page: String,
        x: f64,
        y: f64,
        #[arg(long, short = 'M', default_value = "human")]
        mode: String,
    },
    /// Drag from `(x1, y1)` to `(x2, y2)`.
    #[command(visible_alias = "dr")]
    Drag {
        page: String,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        #[arg(long)]
        token: String,
        #[arg(long, short = 'M', default_value = "human")]
        mode: String,
    },
    /// Prompt the user (in the terminal that ran vs) for a value, then
    /// fill it into a ref. The value is read from tty by the CLI and
    /// shipped to the daemon over the local socket; the agent that
    /// invoked vs prompt-input never sees the bytes. Use `--secret`
    /// for passwords, TANs, and other credentials (terminal echo off).
    #[command(visible_alias = "pi")]
    PromptInput {
        page: String,
        #[arg(value_name = "REF")]
        r: u32,
        #[arg(long)]
        message: String,
        #[arg(long)]
        secret: bool,
        #[arg(long)]
        token: String,
        #[arg(long)]
        group: Option<String>,
    },
    /// Block until the user presses Enter in the terminal. Returns
    /// `ok` on confirm or `! ABORTED` if the user sends EOF / Ctrl-C.
    /// Use as a human-in-loop gate before a sensitive `vs act click`.
    #[command(visible_alias = "pc")]
    PromptConfirm {
        page: String,
        #[arg(long)]
        message: String,
    },
    /// Wire-only `vs_prompt_input` variant — does NOT read from the
    /// local tty. Used by `vs mcp` so an MCP-driven agent can enqueue
    /// a prompt the local user fulfills via `vs pending fulfill`.
    /// Hidden from `--help`: only the MCP server wires it.
    #[command(hide = true)]
    PromptInputQueue {
        page: String,
        r: u32,
        #[arg(long)]
        message: String,
        #[arg(long, default_value_t = false)]
        secret: bool,
        #[arg(long)]
        token: String,
        #[arg(long)]
        group: Option<String>,
        /// Timeout in milliseconds before the daemon gives up waiting
        /// for `vs pending fulfill <id>` (default 5 min).
        #[arg(long = "timeout-ms", default_value_t = 300_000)]
        timeout_ms: u64,
    },
    /// List / fulfill / cancel pending `vs_prompt_input` entries
    /// queued by an MCP-driven agent. Use `vs pending list` to see
    /// what's waiting, `vs pending fulfill <id>` to type the value at
    /// the local tty (`--secret` hides echo), `vs pending cancel <id>`
    /// to abort.
    #[command(visible_alias = "pe")]
    Pending {
        #[command(subcommand)]
        sub: PendingSub,
    },
    /// Run the daemon in this process. The `vs` binary doubles as the
    /// daemon — `vs serve` is what auto-spawn re-execs when the socket
    /// is missing. SIGINT shuts down cleanly.
    Serve {
        /// Send SIGTERM to the running daemon (PID file at
        /// `~/.vibesurfer/daemon.pid`) and wait for the socket to
        /// disappear. Returns immediately if no daemon is running.
        #[arg(long)]
        stop: bool,
    },
    /// Run the MCP (Model Context Protocol) server over stdio.
    /// Speaks JSON-RPC 2.0; each of the 19 vibesurfer primitives is
    /// exposed as one MCP tool. Wire to Claude Desktop / Claude Code
    /// by configuring `vs mcp` as the server command.
    Mcp,
}

#[derive(Debug, Subcommand)]
pub enum PendingSub {
    /// Show all pending `vs_prompt_input` entries the daemon is
    /// holding for fulfillment.
    #[command(visible_alias = "ls")]
    List,
    /// Read the value the daemon is waiting for from the local tty
    /// (rpassword for `--secret`) and send it to the pending entry.
    /// On success the parked `vs_prompt_input` call returns and the
    /// agent observes the filled field.
    #[command(visible_alias = "f")]
    Fulfill {
        /// Pending entry id (from `vs pending list`). If omitted and
        /// there is exactly one pending entry, that one is used.
        id: Option<String>,
    },
    /// Cancel a pending entry. The parked `vs_prompt_input` call
    /// returns `BadRequest "cancelled"`.
    #[command(visible_alias = "c")]
    Cancel {
        id: String,
    },
}

impl Command {
    /// Build the wire [`Request`] for this subcommand. Returns `None`
    /// for commands that the CLI handles locally (none yet).
    #[allow(clippy::too_many_lines)]
    pub fn to_request(&self, session_id: Option<&str>) -> Result<Request> {
        Ok(match self {
            Self::SessionOpen { policy } => {
                let mut r = Request::new("vs_session_open");
                if let Some(p) = policy {
                    r = r.flag_value("policy", p.clone());
                }
                r
            }
            Self::SessionClose => {
                let s = require_session(session_id)?;
                Request::new("vs_session_close").arg(s)
            }
            Self::Open { url } => {
                let s = require_session(session_id)?;
                Request::new("vs_open")
                    .arg(url.clone())
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
            } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_act")
                    .arg(page.clone())
                    .arg(r.to_string())
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
            Self::Capture { page, r, full_page, base64: _ } => {
                // `base64` is a CLI-side post-process — the daemon
                // still returns the on-disk path; dispatch.rs reads
                // the PNG and base64-encodes it before printing.
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
            Self::Viewport { page, spec, dpr } => {
                let s = require_session(session_id)?;
                Request::new("vs_viewport")
                    .arg(page.clone())
                    .arg(spec.clone())
                    .flag_value("session", s)
                    .flag_value("dpr", dpr.to_string())
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
            } => {
                let s = require_session(session_id)?;
                Request::new("vs_click_at")
                    .arg(page.clone())
                    .arg(x.to_string())
                    .arg(y.to_string())
                    .flag_value("session", s)
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
            Self::PromptInput { .. } | Self::PromptConfirm { .. } => {
                anyhow::bail!("vs_prompt_* is local; route via main, not the wire dispatcher");
            }
            Self::PromptInputQueue {
                page, r, message, secret, token, group, timeout_ms,
            } => {
                let s = require_session(session_id)?;
                let mut req = Request::new("vs_prompt_input_queue")
                    .arg(page.clone())
                    .arg(r.to_string())
                    .arg(message.clone())
                    .flag_value("session", s)
                    .flag_value("token", token.clone())
                    .flag_value("timeout-ms", timeout_ms.to_string());
                if *secret { req = req.flag("secret"); }
                if let Some(g) = group { req = req.flag_value("group", g.clone()); }
                req
            }
            Self::Pending { sub } => match sub {
                PendingSub::List => Request::new("vs_pending_list"),
                PendingSub::Fulfill { id } => {
                    // CLI side reads value from tty before sending the
                    // wire request — handled in dispatch.rs. Here we
                    // just stub a placeholder; dispatch overrides the
                    // value arg with what the user typed.
                    let id_v = id.clone().unwrap_or_default();
                    Request::new("vs_pending_fulfill").arg(id_v).arg(String::new())
                }
                PendingSub::Cancel { id } => Request::new("vs_pending_cancel").arg(id.clone()),
            },
            Self::Serve { .. } => {
                anyhow::bail!("vs_serve is local; route via main, not the wire dispatcher");
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
            Self::SessionOpen { .. } | Self::Status | Self::Serve { .. } | Self::Mcp | Self::Pending { .. }
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
        "d" => "dom",
        "p" => "performance",
        "ce" => "cookie-events",
        other => other,
    }
}

mod dispatch;
mod render;

pub use dispatch::{connect, resolve_paths, resolve_session, run};
pub use render::render;
