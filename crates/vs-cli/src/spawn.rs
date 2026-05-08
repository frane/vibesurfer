//! Daemon auto-spawn.
//!
//! `vs` and the daemon are the same binary. When the CLI runs and the
//! daemon socket is missing, [`spawn_daemon`] re-execs `vs serve`
//! detached from the current process. The CLI then waits for the
//! socket to appear (with a timeout). Failure to spawn or connect
//! within the budget yields a local error (the user sees
//! `! DAEMON_START_FAILED`-equivalent stderr).

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

/// Path to the `vs` binary that should run the daemon. Resolution
/// order:
///
/// 1. `$VS_DAEMON_BIN` — explicit override (tests use this).
/// 2. `current_exe()` — re-exec ourselves with `vs serve`. The CLI is
///    a single binary, so this is the production path.
/// 3. `vs` on `$PATH` — last-resort fallback if `current_exe()` fails.
#[must_use]
pub fn daemon_binary() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("VS_DAEMON_BIN") {
        return p.into();
    }
    if let Ok(self_exe) = std::env::current_exe() {
        return self_exe;
    }
    "vs".into()
}

/// Spawn `vs serve` detached. Inherits no stdio so the CLI process is
/// not kept alive by the daemon's pipes.
///
/// `extra_args` are passed *after* `serve` (e.g. `--home=/tmp/x`).
pub fn spawn_daemon(extra_args: &[&str]) -> Result<()> {
    let bin = daemon_binary();
    Command::new(&bin)
        .arg("serve")
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn {} serve", bin.display()))?;
    Ok(())
}

/// Wait until a daemon is reachable at `socket`, polling at ~50ms.
/// On Unix this checks for the AF_UNIX socket file; on Windows it
/// connect-probes the named pipe (pipes don't appear on the
/// filesystem). Returns `Ok(())` on success, error on timeout.
pub fn wait_for_socket(socket: impl AsRef<Path>, timeout: Duration) -> Result<()> {
    let socket = socket.as_ref();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if vs_daemon::transport::is_listening(socket) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!(
        "daemon socket {} did not appear within {:?}",
        socket.display(),
        timeout
    )
}
