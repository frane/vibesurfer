//! End-to-end CLI dispatch: resolve paths/session, connect to the
//! daemon (auto-spawning if needed), build the wire request, send it,
//! and side-effect on session-open / session-close.

use std::path::PathBuf;

use anyhow::{Context as _, Result};

use super::{Cli, Command};
use crate::active_session;
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

/// Resolve the active session id from CLI override or the on-disk
/// pointer file.
pub fn resolve_session(cli: &Cli, paths: &Paths) -> Result<Option<String>> {
    if let Some(s) = &cli.session {
        return Ok(Some(s.clone()));
    }
    active_session::read(paths.active_session())
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

/// End-to-end dispatch: parse session, connect, send, side-effect on
/// `session-open` (write active-session file). Returns the response.
pub fn run(cli: &Cli) -> Result<Response> {
    let paths = resolve_paths(cli.home.as_ref());
    let session_id = resolve_session(cli, &paths)?;
    let mut client = connect(cli, &paths)?;
    let req = cli.command.to_request(session_id.as_deref())?;
    let resp = client.call(&req).context("daemon call")?;
    match (&cli.command, &resp.envelope) {
        (Command::SessionOpen { .. }, vs_protocol::Envelope::Success(_)) => {
            if let Some(line) = resp.body.first() {
                active_session::write(paths.active_session(), line.trim())?;
            }
        }
        (Command::SessionClose, vs_protocol::Envelope::Success(_)) => {
            active_session::clear(paths.active_session())?;
        }
        _ => {}
    }
    Ok(resp)
}
