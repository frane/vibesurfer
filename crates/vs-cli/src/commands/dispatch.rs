//! End-to-end CLI dispatch: resolve paths/session, connect to the
//! daemon (auto-spawning if needed), build the wire request, send it,
//! and side-effect on session-open / session-close.
//!
//! Session resolution order (v0.1.7+):
//!   1. `--session=<id>` (or `-S`) — explicit override
//!   2. `VS_SESSION` env var — set by the caller's shell
//!   3. Per-caller saved session at `~/.vibesurfer/callers/<key>` —
//!      keyed by the POSIX session id on Unix (parent pid elsewhere)
//!      so different shells / agents get independent sessions
//!      automatically. See [`crate::caller`] for why it is the session
//!      id and not the parent pid. Bindings unused for 30 days are
//!      reaped when a new one is written.
//!   4. Auto-create: if the command needs a session and none of the
//!      above resolved, the CLI implicitly runs `vs_session_open`
//!      first and binds the new id to the caller key
//!
//! The legacy `active-session` pointer file is no longer read or
//! written; concurrent agents would race on it.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;

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
        spawn::wait_for_socket(&socket, std::time::Duration::from_secs(10))?;
    }
    Client::connect(&socket)
}

fn save_caller_session(paths: &Paths, key: &str, session_id: &str) -> Result<()> {
    let path = paths.caller_session(key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create callers/ directory")?;
        prune_caller_sessions(parent);
    }
    std::fs::write(&path, session_id).context("write caller session file")?;
    Ok(())
}

/// How long a caller binding outlives its last use. A terminal session
/// or agent that has not run `vs` in this long is not coming back to
/// the same key.
const CALLER_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// Drop caller bindings nothing has touched in [`CALLER_TTL`].
///
/// The directory had no reaper, so every key that ever ran `vs` left a
/// file behind for good — 301 of them on the author's machine, most
/// from ephemeral keys that could never be looked up again. Runs only
/// when a binding is written (session open / auto-create), which is
/// rare compared to ordinary calls, and never fails a command: a
/// directory we cannot read or a file we cannot remove is left alone.
fn prune_caller_sessions(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let stale = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > CALLER_TTL);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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
#[allow(clippy::too_many_lines)]
pub fn run(cli: &Cli) -> Result<Response> {
    let paths = resolve_paths(cli.home.as_ref());

    // `vs capture clean` is a pure local filesystem op — it needs no
    // session and no daemon, so handle it before connecting (which would
    // otherwise auto-spawn a daemon just to delete files).
    if let Command::Capture {
        clean: Some(sub), ..
    } = &cli.command
    {
        return run_capture_clean(&paths, sub);
    }

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

    // Local prompt primitives: read from the user's tty before any
    // wire call. The value (PromptInput) or confirmation (PromptConfirm)
    // is collected by the CLI in the user's terminal; the agent that
    // invoked vs prompt-input never sees the bytes.
    match &cli.command {
        // `vs auth import <name> <file>`: read the session blob file
        // here (CLI-side) and ship it base64-encoded so the JSON body,
        // which contains newlines, can't break the line protocol.
        Command::Auth { sub, rest } if sub == "import" => {
            let name = rest
                .first()
                .context("vs auth import: missing <name>")?
                .clone();
            let file = rest.get(1).context("vs auth import: missing <file>")?;
            let bytes =
                std::fs::read(file).with_context(|| format!("read auth blob file {file}"))?;
            let b64 = STANDARD.encode(&bytes);
            let s = session_id
                .clone()
                .context("vs auth import: no active session")?;
            let req = vs_protocol::Request::new("vs_auth")
                .arg("import")
                .arg(name)
                .arg(b64)
                .flag_value("session", s);
            return client.call(&req).context("daemon call (auth import)");
        }
        Command::PromptInput {
            page,
            r,
            message,
            secret,
            token,
            group,
        } => {
            return run_prompt_input(
                &mut client,
                session_id.as_deref(),
                page,
                *r,
                message,
                *secret,
                token,
                group.as_deref(),
            );
        }
        Command::PromptConfirm { page: _, message } => {
            read_user_confirm(message)?;
            return Ok(Response {
                envelope: vs_protocol::Envelope::Success(vs_protocol::StateToken([0u8; 8])),
                body: Vec::new(),
                warnings: Vec::new(),
            });
        }
        Command::PromptForm {
            page,
            fields,
            token,
            group,
            open,
            no_wait,
            timeout_ms,
        } => {
            return run_prompt_form(
                &mut client,
                session_id.as_deref(),
                page,
                fields,
                token,
                group.as_deref(),
                *open,
                *no_wait,
                *timeout_ms,
            );
        }
        Command::PromptScan {
            page,
            message,
            open,
        } => {
            // Mint a live-view URL so the human can see the QR or 2FA
            // screen of the headless page, then wait for them to
            // finish the step out of band.
            let session = session_id.as_deref().unwrap_or_default();
            let req = vs_protocol::Request::new("vs_watch")
                .arg(page.clone())
                .flag_value("session", session);
            let resp = client
                .call(&req)
                .context("daemon call (prompt-scan watch)")?;
            if let vs_protocol::Envelope::Error { .. } = &resp.envelope {
                return Ok(resp);
            }
            let url = body_value(&resp, "url").context("prompt-scan: no url in response")?;
            eprintln!("vs prompt-scan: open {url} to view the page. {message}");
            if *open {
                open_in_browser(&url);
            }
            read_user_confirm(message)?;
            return Ok(Response {
                envelope: vs_protocol::Envelope::Success(vs_protocol::StateToken([0u8; 8])),
                body: Vec::new(),
                warnings: Vec::new(),
            });
        }
        Command::Pending {
            sub: super::PendingSub::Fulfill { id },
        } => {
            return run_pending_fulfill(&mut client, id.clone());
        }
        _ => {}
    }

    let req = cli.command.to_request(session_id.as_deref())?;
    let mut resp = client.call(&req).context("daemon call")?;

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
        (Command::Watch { open: true, .. }, vs_protocol::Envelope::Success(_)) => {
            if let Some(u) = body_value(&resp, "url") {
                open_in_browser(&u);
            }
        }
        (Command::Capture { base64: true, .. }, vs_protocol::Envelope::Success(_)) => {
            // The body's first line is the on-disk PNG path. Read it
            // and replace the body with `base64=<bytes>` plus the
            // original `path=…` so MCP-driven agents can ship pixels
            // inline without losing the disk artifact.
            if let Some(path_line) = resp.body.first().cloned() {
                let path = std::path::PathBuf::from(path_line.trim());
                if let Ok(bytes) = std::fs::read(&path) {
                    use base64::engine::general_purpose::STANDARD;
                    use base64::Engine as _;
                    let b64 = STANDARD.encode(&bytes);
                    resp.body = vec![format!("base64={b64}"), format!("path={}", path.display())];
                }
            }
        }
        _ => {}
    }
    Ok(resp)
}

/// Dispatch `vs prompt-input`. With a controlling tty, read the value
/// locally and fill it. Without one (the common agent case), enqueue a
/// pending entry and park until a local human runs `vs pending fulfill`
/// — mirroring the MCP `vs_prompt_input` path — instead of hard-erroring
/// on the missing tty.
#[allow(clippy::too_many_arguments)]
fn run_prompt_input(
    client: &mut Client,
    session_id: Option<&str>,
    page: &str,
    r: u32,
    message: &str,
    secret: bool,
    token: &str,
    group: Option<&str>,
) -> Result<Response> {
    let session = session_id.unwrap_or_default();
    if has_local_tty() {
        let value = read_user_input(message, secret)?;
        let mut req = vs_protocol::Request::new("vs_act")
            .arg(page)
            .arg(r.to_string())
            .arg("fill")
            .arg(value)
            .flag_value("session", session)
            .flag_value("token", token);
        if let Some(g) = group {
            req = req.flag_value("group", g);
        }
        return client.call(&req).context("daemon call");
    }
    let url_note = client
        .call(&vs_protocol::Request::new("vs_pending_url"))
        .ok()
        .and_then(|r| body_value(&r, "url"))
        .map_or_else(String::new, |u| format!(", or open {u} in a browser"));
    eprintln!(
        "vs prompt-input: no local tty — enqueued a pending entry; \
         run `vs pending fulfill` at a terminal{url_note} \
         (parks up to 5 min)"
    );
    let mut req = vs_protocol::Request::new("vs_prompt_input_queue")
        .arg(page)
        .arg(r.to_string())
        .arg(message)
        .flag_value("session", session)
        .flag_value("token", token);
    if secret {
        req = req.flag("secret");
    }
    if let Some(g) = group {
        req = req.flag_value("group", g);
    }
    client.call(&req).context("daemon call (pending queue)")
}

/// Extract `key\t<value>` from a wire response body.
fn body_value(resp: &Response, key: &str) -> Option<String> {
    resp.body.iter().find_map(|line| {
        line.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('\t'))
            .map(|v| v.trim().to_string())
    })
}

/// Dispatch `vs prompt-form`: enqueue all fields, surface the browser
/// entry URL (stderr note; `--open` also launches the browser), then
/// park in `vs_prompt_form_wait` until the human submits — unless
/// `--no-wait`, which returns the enqueue response (form id + URL) so
/// the caller can park later.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn run_prompt_form(
    client: &mut Client,
    session_id: Option<&str>,
    page: &str,
    fields: &[String],
    token: &str,
    group: Option<&str>,
    open: bool,
    no_wait: bool,
    timeout_ms: u64,
) -> Result<Response> {
    let session = session_id.unwrap_or_default();
    let mut req = vs_protocol::Request::new("vs_prompt_form")
        .arg(page)
        .flag_value("session", session)
        .flag_value("token", token);
    for spec in fields {
        let (r, rest) = spec
            .split_once('=')
            .with_context(|| format!("bad --field {spec:?}: want REF=LABEL[,secret]"))?;
        r.parse::<u32>()
            .with_context(|| format!("bad --field {spec:?}: ref is not a number"))?;
        let (label, secret) = match rest.strip_suffix(",secret") {
            Some(l) => (l, true),
            None => (rest, false),
        };
        if label.is_empty() {
            anyhow::bail!("bad --field {spec:?}: empty label");
        }
        req = req.arg(format!("{r}:{}:{label}", u8::from(secret)));
    }
    if let Some(g) = group {
        req = req.flag_value("group", g);
    }
    let enqueue = client.call(&req).context("daemon call (prompt-form)")?;
    if let vs_protocol::Envelope::Error { .. } = &enqueue.envelope {
        return Ok(enqueue);
    }
    let form_id = body_value(&enqueue, "form").context("prompt-form: no form id in response")?;
    let url = body_value(&enqueue, "url").context("prompt-form: no url in response")?;
    eprintln!(
        "vs prompt-form: {} field(s) — open {url} (single-use, 10 min)",
        fields.len(),
    );
    if open {
        open_in_browser(&url);
    }
    if no_wait {
        return Ok(enqueue);
    }
    let wait = vs_protocol::Request::new("vs_prompt_form_wait")
        .arg(form_id)
        .flag_value("session", session)
        .flag_value("timeout-ms", timeout_ms.to_string());
    client.call(&wait).context("daemon call (prompt-form wait)")
}

/// Best-effort `open <url>` in the platform default browser. Failure
/// is fine — the URL is already printed.
fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    let _ = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Whether this process has a controlling terminal to read a prompt
/// from. On Unix `rpassword`/the confirm path read `/dev/tty`, so the
/// real question is whether that opens — a non-interactive agent shell
/// has none (open fails with `ENXIO`/"Device not configured"). Used to
/// fall back to the pending-queue path instead of hard-erroring.
fn has_local_tty() -> bool {
    #[cfg(unix)]
    {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .is_ok()
    }
    #[cfg(not(unix))]
    {
        use std::io::IsTerminal as _;
        std::io::stdin().is_terminal()
    }
}

/// Prompt the user (via tty) and return the value. When `secret` is
/// true, terminal echo is disabled and the input is read through
/// `rpassword`.
fn read_user_input(message: &str, secret: bool) -> Result<String> {
    use std::io::Write as _;
    let mut stderr = std::io::stderr();
    if secret {
        // rpassword writes its own prompt and reads from /dev/tty so
        // it works even when stdin is redirected.
        let v = rpassword::prompt_password(format!("{message} ")).context("read secret")?;
        Ok(v)
    } else {
        write!(stderr, "{message} ").ok();
        stderr.flush().ok();
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf).context("read line")?;
        // Strip the trailing newline; leading whitespace stays.
        Ok(buf.trim_end_matches(['\r', '\n']).to_string())
    }
}

/// Block until the user presses Enter at the tty. EOF / Ctrl-D abort.
fn read_user_confirm(message: &str) -> Result<()> {
    use std::io::Write as _;
    let mut stderr = std::io::stderr();
    write!(stderr, "{message} [Enter to confirm, Ctrl-C to abort] ").ok();
    stderr.flush().ok();
    let mut buf = String::new();
    let n = std::io::stdin()
        .read_line(&mut buf)
        .context("read confirm")?;
    if n == 0 {
        anyhow::bail!("ABORTED: stdin closed before confirm");
    }
    Ok(())
}

/// Prune the captures directory per `vs capture clean` flags. Runs
/// entirely CLI-side against `paths.captures()` — no daemon required.
fn run_capture_clean(paths: &Paths, sub: &super::CaptureSub) -> Result<Response> {
    use vs_daemon::captures::{self, RetentionPolicy};
    let super::CaptureSub::Clean {
        all,
        older_than,
        keep,
    } = sub;

    let dir = paths.captures();
    let policy = if *all {
        RetentionPolicy {
            all: true,
            ..Default::default()
        }
    } else if older_than.is_some() || keep.is_some() {
        let max_age =
            match older_than {
                Some(s) => Some(captures::parse_duration(s).with_context(|| {
                    format!("bad --older-than {s:?} (use e.g. 7d, 12h, 30m, 90s)")
                })?),
                None => None,
            };
        RetentionPolicy {
            all: false,
            keep: *keep,
            max_age,
        }
    } else {
        // No flags: apply the same default cap the daemon auto-enforces.
        RetentionPolicy::default_cap()
    };

    let stats = captures::prune(&dir, &policy, std::time::SystemTime::now());
    let body = vec![format!(
        "deleted={} freed_kib={} kept={} dir={}",
        stats.deleted,
        stats.freed_bytes / 1024,
        stats.kept,
        dir.display()
    )];
    Ok(Response {
        envelope: vs_protocol::Envelope::Success(vs_protocol::StateToken([0u8; 8])),
        body,
        warnings: Vec::new(),
    })
}

/// Resolve a pending entry id, read the value from the local tty,
/// and send the fulfill RPC. Extracted from `run()` so clippy's
/// `too_many_lines` lint stays satisfied.
fn run_pending_fulfill(client: &mut Client, id: Option<String>) -> Result<Response> {
    let resolved_id = if let Some(s) = id {
        s
    } else {
        let list_req = vs_protocol::Request::new("vs_pending_list");
        let list_resp = client.call(&list_req).context("pending list")?;
        let ids: Vec<&str> = list_resp
            .body
            .iter()
            .filter_map(|l| l.split('\t').next())
            .filter(|s| !s.is_empty())
            .collect();
        match ids.len() {
            0 => anyhow::bail!("no pending entries to fulfill"),
            1 => ids[0].to_string(),
            n => anyhow::bail!("{n} pending entries — pass an explicit id"),
        }
    };
    let peek_req = vs_protocol::Request::new("vs_pending_peek").arg(resolved_id.clone());
    let peek_resp = client.call(&peek_req).context("pending peek")?;
    let line = peek_resp.body.first().cloned().unwrap_or_default();
    let parts: Vec<&str> = line.split('\t').collect();
    let message = parts.get(4).copied().unwrap_or("value");
    let secret = parts.get(3).copied() == Some("1");
    let value = read_user_input(message, secret)?;
    let req = vs_protocol::Request::new("vs_pending_fulfill")
        .arg(resolved_id)
        .arg(value);
    client.call(&req).context("daemon call")
}

#[cfg(test)]
mod tests {
    use super::{prune_caller_sessions, CALLER_TTL};

    /// The `callers/` directory reaps bindings nothing has used in a
    /// month. It had no reaper at all, so every ephemeral key that ever
    /// ran `vs` left a file behind for good — 301 of them on the
    /// author's machine, most of them keys that could never be looked
    /// up again.
    #[test]
    fn stale_caller_bindings_are_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("sid-1234");
        let stale = dir.path().join("9999-1700000000");
        std::fs::write(&fresh, "s_fresh").unwrap();
        std::fs::write(&stale, "s_stale").unwrap();

        let old = std::time::SystemTime::now() - (CALLER_TTL + std::time::Duration::from_secs(60));
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();

        prune_caller_sessions(dir.path());

        assert!(fresh.exists(), "a recently-used binding must survive");
        assert!(
            !stale.exists(),
            "a binding unused past the TTL must be reaped"
        );
    }
}
