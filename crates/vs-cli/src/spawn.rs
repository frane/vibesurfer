//! Daemon auto-spawn.
//!
//! `vs` and the daemon are the same binary. When the CLI runs and the
//! daemon socket is missing, [`spawn_daemon`] re-execs `vs serve`
//! detached from the current process, its stderr appended to
//! `daemon.log` in the home. The CLI then waits for the socket while
//! the daemon runs; a daemon that dies or never binds yields a local
//! error with the end of that log.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::process::{Child, Command, Stdio};
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

/// The log is cut back to nothing once it passes this, at the next spawn.
const LOG_CAP: u64 = 8 * 1024 * 1024;

/// How long a daemon we spawned may take to bind its socket while it
/// is still running. A cold start (first run, libraries not yet in the
/// page cache) can take well over the few hundred milliseconds a warm
/// one does.
pub const START_BUDGET: Duration = Duration::from_secs(60);

/// Spawn `vs serve` detached, with its stderr appended to `log` so a
/// daemon that fails leaves a trace. Inherits no other stdio so the CLI
/// process is not kept alive by the daemon's pipes.
///
/// `extra_args` are passed *after* `serve` (e.g. `--home=/tmp/x`).
/// Appending rather than rotating keeps a concurrent spawn, which exits
/// once it sees the first daemon's lock, from moving that daemon's log.
pub fn spawn_daemon(extra_args: &[&str], log: &Path) -> Result<Child> {
    let bin = daemon_binary();
    Command::new(&bin)
        .arg("serve")
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(open_log(log).map_or_else(|_| Stdio::null(), Stdio::from))
        .spawn()
        .with_context(|| format!("spawn {} serve", bin.display()))
}

fn open_log(log: &Path) -> std::io::Result<File> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(log).is_ok_and(|m| m.len() > LOG_CAP) {
        let _ = File::create(log);
    }
    OpenOptions::new().create(true).append(true).open(log)
}

/// Wait until a daemon is reachable at `socket`, polling at ~50ms.
/// On Unix this checks for the AF_UNIX socket file; on Windows it
/// connect-probes the named pipe (pipes don't appear on the
/// filesystem).
///
/// `daemon` is the one we spawned. If it fails, the error says so at
/// once, with the end of its log, instead of waiting out the budget.
/// If it exits cleanly, another daemon holds the instance lock, and
/// this waits for that one's socket.
pub fn wait_for_socket(
    socket: impl AsRef<Path>,
    daemon: &mut Child,
    log: &Path,
    timeout: Duration,
) -> Result<()> {
    let socket = socket.as_ref();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if vs_daemon::transport::is_listening(socket) {
            return Ok(());
        }
        if let Ok(Some(status)) = daemon.try_wait() {
            if !status.success() {
                anyhow::bail!(
                    "the daemon exited ({status}) before listening on {}\n{}",
                    socket.display(),
                    log_tail(log)
                );
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!(
        "daemon socket {} did not appear within {:?}\n{}",
        socket.display(),
        timeout,
        log_tail(log)
    )
}

/// The last lines of the daemon log, for an error message.
fn log_tail(log: &Path) -> String {
    const LINES: usize = 20;
    let Ok(text) = std::fs::read_to_string(log) else {
        return format!("(no log at {})", log.display());
    };
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(LINES)..].join("\n");
    format!("last lines of {}:\n{tail}", log.display())
}
