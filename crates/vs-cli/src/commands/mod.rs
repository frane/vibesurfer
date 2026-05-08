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
    #[arg(long, global = true)]
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
    #[arg(long, global = true)]
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
        #[arg(long)]
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
        #[arg(long)]
        page: Option<String>,
        #[arg(long)]
        group: Option<String>,
        #[arg(long)]
        since: Option<i64>,
        #[arg(long)]
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
    #[command(visible_alias = "in")]
    Inspect {
        page: String,
        kind: String,
        #[arg(num_args = 0..=3)]
        rest: Vec<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        level: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        max: Option<String>,
        #[arg(long)]
        full: bool,
        #[arg(long = "unsafe-log")]
        unsafe_log: bool,
    },
    /// Run the daemon in this process. The `vs` binary doubles as the
    /// daemon — `vs serve` is what auto-spawn re-execs when the socket
    /// is missing. SIGINT shuts down cleanly.
    Serve,
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
            Self::Capture { page, r, full_page } => {
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
                let mut req = Request::new("vs_inspect")
                    .arg(kind.clone())
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
            Self::Serve => {
                anyhow::bail!("vs_serve is local; route via main, not the wire dispatcher");
            }
        })
    }

    /// True if this subcommand requires an active session.
    #[must_use]
    pub fn needs_session(&self) -> bool {
        !matches!(self, Self::SessionOpen { .. } | Self::Status | Self::Serve)
    }
}

fn require_session(session: Option<&str>) -> Result<String> {
    session
        .map(str::to_string)
        .context("no active session — run `vs session-open` or pass `--session=<id>`")
}

mod dispatch;
mod render;

pub use dispatch::{connect, resolve_paths, resolve_session, run};
pub use render::render;
