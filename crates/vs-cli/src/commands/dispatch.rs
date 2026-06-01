//! End-to-end CLI dispatch: resolve paths/session, connect to the
//! daemon (auto-spawning if needed), build the wire request, send it,
//! and side-effect on session-open / session-close.
//!
//! Session resolution order (v0.1.7+):
//!   1. `--session=<id>` (or `-S`) — explicit override
//!   2. `VS_SESSION` env var — set by the caller's shell
//!   3. Per-caller saved session at `~/.vibesurfer/callers/<key>` —
//!      keyed by `<parent_pid>-<parent_start_time>` so different
//!      shells / agents get independent sessions automatically
//!   4. Auto-create: if the command needs a session and none of the
//!      above resolved, the CLI implicitly runs `vs_session_open`
//!      first and binds the new id to the caller key
//!
//! The legacy `active-session` pointer file is no longer read or
//! written; concurrent agents would race on it.

use std::path::PathBuf;

use anyhow::{Context as _, Result};

use super::{Cli, Command};
use crate::caller;
use crate::client::{Client, Response};
use crate::paths::Paths;
use crate::spawn;

/// Resolve effective paths from `--home` or `$HOME`.
#[must_use]
pub fn resolve_paths(home_override: Option<&PathBuf>) -> Paths {
    match home_override {
        Some(p) => Paths::at(p.clone()),
        None => Paths::home(),
    }
}

/// Resolve the session id without auto-creating one. Returns `None` if
/// no explicit override / env var is set and the caller has no saved
/// session yet — [`run`] handles the auto-create case for commands
/// that need a session.
pub fn resolve_session(cli: &Cli, paths: &Paths) -> Result<Option<String>> {
    if let Some(s) = &cli.session {
        return Ok(Some(s.clone()));
    }
    if let Ok(s) = std::env::var("VS_SESSION") {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.to_string()));
        }
    }
    if let Some(key) = caller::caller_key() {
        let p = paths.caller_session(&key);
        if let Ok(contents) = std::fs::read_to_string(&p) {
            let trimmed = contents.trim();
            if !trimmed.is_empty() {
                return Ok(Some(trimmed.to_string()));
            }
        }
    }
    Ok(None)
}

/// Connect to the daemon, auto-spawning if necessary (unless
/// `--no-spawn`). When the caller passed `--home`, propagate it to
/// the spawned daemon — otherwise auto-spawn writes the socket to
/// the default home and the caller waits forever for it to appear at
/// the requested home.
pub fn connect(cli: &Cli, paths: &Paths) -> Result<Client> {
    let socket = cli.socket.clone().unwrap_or_else(|| paths.socket());
    if !vs_daemon::transport::is_listening(&socket) && !cli.no_spawn {
        let mut extra: Vec<String> = Vec::new();
        if let Some(home) = cli.home.as_ref() {
            extra.push(format!("--home={}", home.display()));
        }
        let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
        spawn::spawn_daemon(&extra_refs)?;
        spawn::wait_for_socket(&socket, std::time::Duration::from_secs(2))?;
    }
    Client::connect(&socket)
}

fn save_caller_session(paths: &Paths, key: &str, session_id: &str) -> Result<()> {
    let path = paths.caller_session(key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create callers/ directory")?;
    }
    std::fs::write(&path, session_id).context("write caller session file")?;
    Ok(())
}

/// Sentinel written to the caller file by `vs session-close` so a
/// follow-up command does NOT silently auto-create a fresh session.
/// Explicit close means "I'm done" — a subsequent `vs open` should
/// fail loudly, the same way it did pre-v0.1.7. The user re-opens
/// by running `vs session-open` explicitly.
const CLOSED_SENTINEL: &str = "__closed__";

fn mark_caller_closed(paths: &Paths, key: &str) -> Result<()> {
    save_caller_session(paths, key, CLOSED_SENTINEL)
}

/// End-to-end dispatch. Auto-opens a session for the caller if needed
/// and binds session-open / session-close side effects to the caller
/// key so concurrent agents in different shells stay isolated.
pub fn run(cli: &Cli) -> Result<Response> {
    let paths = resolve_paths(cli.home.as_ref());
    let mut session_id = resolve_session(cli, &paths)?;
    let mut client = connect(cli, &paths)?;
    let caller_key = caller::caller_key();

    // Explicit close sentinel means "user said they were done" — don't
    // auto-reopen on a follow-up command. Treat it as no session so the
    // daemon returns the same `NotFound` error the pre-v0.1.7 active-
    // session file used to surface.
    let explicit_close = matches!(session_id.as_deref(), Some(CLOSED_SENTINEL));
    if explicit_close {
        session_id = None;
    }

    // Auto-open: if this command needs a session and none was resolved,
    // open one transparently and remember it for the caller. Excluded:
    // SessionOpen (about to open anyway), and the explicit-close case
    // (the user told us to stop).
    if session_id.is_none()
        && !explicit_close
        && cli.command.needs_session()
        && !matches!(cli.command, Command::SessionOpen { .. })
    {
        let open_req = vs_protocol::Request::new("vs_session_open");
        let open_resp = client.call(&open_req).context("auto session-open")?;
        if let vs_protocol::Envelope::Success(_) = &open_resp.envelope {
            if let Some(line) = open_resp.body.first() {
                let id = line.trim().to_string();
                if let Some(key) = caller_key.as_ref() {
                    let _ = save_caller_session(&paths, key, &id);
                }
                session_id = Some(id);
            }
        }
    }

    let req = cli.command.to_request(session_id.as_deref())?;
    let resp = client.call(&req).context("daemon call")?;

    match (&cli.command, &resp.envelope) {
        (Command::SessionOpen { .. }, vs_protocol::Envelope::Success(_)) => {
            if let (Some(line), Some(key)) = (resp.body.first(), caller_key.as_ref()) {
                let _ = save_caller_session(&paths, key, line.trim());
            }
        }
        (Command::SessionClose, vs_protocol::Envelope::Success(_)) => {
            if let Some(key) = caller_key.as_ref() {
                let _ = mark_caller_closed(&paths, key);
            }
        }
        _ => {}
    }
    Ok(resp)
}
