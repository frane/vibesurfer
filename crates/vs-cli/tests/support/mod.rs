//! Shared test helpers for M6 integration tests.
//!
//! Drives the **real `vs` binary** against a fixture HTTP server.
//! Same harness shape across all three backends; per-backend
//! differences live in the `Backend` enum and the gating in
//! `Backend::available_on_current_platform()`.

#![allow(dead_code)]

pub mod fixture_server;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub use fixture_server::FixtureServer;

const VS_BIN: &str = env!("CARGO_BIN_EXE_vs");

/// Which engine backend a test is targeting. Currently only `Mac` is
/// available on this host; Linux + Windows variants land in their
/// respective M6 transactions.
#[derive(Copy, Clone, Debug)]
pub enum Backend {
    Mac,
    Linux,
    Windows,
}

impl Backend {
    /// Whether this backend can be exercised in the current process.
    /// On a Mac host: only `Mac`. On Linux: only `Linux`. The Windows
    /// branch is always `false` until the Docker / CI harnesses pick
    /// up the conditional in their own transactions.
    pub fn available_on_current_platform(self) -> bool {
        match self {
            Self::Mac => cfg!(target_os = "macos"),
            Self::Linux => cfg!(target_os = "linux"),
            Self::Windows => cfg!(target_os = "windows"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Mac => "mac",
            Self::Linux => "linux",
            Self::Windows => "windows",
        }
    }
}

/// Iterate over every backend that's exercisable on this host. Tests
/// loop over this so the same body runs on its native platform.
pub fn each_available_backend() -> impl Iterator<Item = Backend> {
    [Backend::Mac, Backend::Linux, Backend::Windows]
        .into_iter()
        .filter(|b| b.available_on_current_platform())
}

/// Owns the spawned `vs serve` child process. Killed on drop.
pub struct DaemonGuard {
    child: Child,
    home: PathBuf,
}

impl DaemonGuard {
    pub fn home(&self) -> &Path {
        &self.home
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        // Stop the daemon gracefully (SIGTERM) so its engine reaps the
        // page render processes on exit. A plain SIGKILL skips the
        // shutdown path and orphans a ~90 MB web-content process per open
        // page — across a full test run that piled into gigabytes.
        #[cfg(unix)]
        {
            let pid = self.child.id().to_string();
            let _ = Command::new("kill").arg("-TERM").arg(&pid).status();
            for _ in 0..60 {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn a real `vs serve` daemon backed by the host's native engine
/// (WkBackend on macOS, WpeBackend on Linux). The daemon's home dir
/// is provided by caller (typically a `tempfile::TempDir`).
pub fn spawn_daemon(home: &Path) -> DaemonGuard {
    spawn_daemon_with_env(home, &[])
}

/// Variant of [`spawn_daemon`] that sets extra environment variables
/// on the child process. Used by tests that exercise capability-flag
/// gates (e.g. `VS_DISABLE_INSPECTOR=1`) where the engine's behavior
/// changes based on construction-time env.
pub fn spawn_daemon_with_env(home: &Path, env: &[(&str, &str)]) -> DaemonGuard {
    // Pipe daemon stdout/stderr to files inside `home` so test
    // failures have something to grep. The previous /dev/null setup
    // hid every panic the daemon emitted on first request — Windows
    // engine-tests in particular were silent about why every cell
    // failed.
    let log = home.join("daemon.log");
    let log_file = std::fs::File::create(&log).expect("create daemon log");
    let log_clone = log_file.try_clone().expect("clone daemon log fd");
    let mut cmd = Command::new(VS_BIN);
    cmd.env("RUST_BACKTRACE", "1")
        .arg(format!("--home={}", home.display()))
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_clone));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("spawn vs serve");
    let mut guard = DaemonGuard {
        child,
        home: home.to_path_buf(),
    };
    let socket = home.join("daemon.sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if vs_daemon::transport::is_listening(&socket) {
            return guard;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = guard.child.kill();
    let _ = guard.child.wait();
    panic!(
        "daemon socket {} did not appear within 10s",
        socket.display()
    );
}

/// Output of one CLI invocation — exit code + stdout + stderr.
pub struct CliOut {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Run the `vs` CLI with the given args against the daemon at `home`.
pub fn vs(home: &Path, args: &[&str]) -> CliOut {
    let out = Command::new(VS_BIN)
        .arg(format!("--home={}", home.display()))
        .arg("--no-spawn")
        .args(args)
        .output()
        .expect("run vs");
    let mut stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // If the call failed and the daemon dropped the connection,
    // append the daemon's own log so the test failure message
    // surfaces the underlying panic / bind error / engine crash.
    let code = out.status.code().unwrap_or(-1);
    if code != 0 && stderr.contains("daemon closed connection") {
        let log_path = home.join("daemon.log");
        if let Ok(log) = std::fs::read_to_string(&log_path) {
            if !log.trim().is_empty() {
                stderr.push_str("\n--- daemon.log ---\n");
                stderr.push_str(&log);
            }
        }
    }
    CliOut {
        code,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr,
    }
}

/// Pull `<token>` out of the first `@<token>` line in stdout. Panics
/// if no success envelope appears.
pub fn token_of(out: &CliOut) -> String {
    for line in out.stdout.lines() {
        if let Some(rest) = line.strip_prefix('@') {
            return rest
                .split_whitespace()
                .next()
                .expect("@-line without token")
                .to_string();
        }
    }
    panic!(
        "no success envelope in stdout (code={}):\nstdout:\n{}\nstderr:\n{}",
        out.code, out.stdout, out.stderr
    );
}

/// First body line — the line after the envelope. Used for primitives
/// that return a single payload line (session-open, open, read).
pub fn body_first(out: &CliOut) -> String {
    let mut lines = out.stdout.lines();
    let _envelope = lines.next();
    match lines.next() {
        Some(line) => line.to_string(),
        None => panic!("empty body in:\n{}", out.stdout),
    }
}

/// Body lines after the envelope, joined verbatim. Used for primitives
/// that return multiple lines (view, log, find, inspect).
pub fn body_rest(out: &CliOut) -> String {
    let mut lines = out.stdout.lines();
    let _envelope = lines.next();
    lines.collect::<Vec<_>>().join("\n")
}

/// Assert clean success (exit 0) or success-with-warnings (exit 2).
/// Both are pass states per `docs/PROTOCOL.md`.
pub fn assert_ok(label: &str, out: &CliOut) {
    assert!(
        out.code == 0 || out.code == 2,
        "{label}: exit={} stdout={:?} stderr={:?}",
        out.code,
        out.stdout,
        out.stderr
    );
}

/// Assert error envelope. Returns the error code so callers can match.
pub fn assert_err(out: &CliOut) -> String {
    for line in out.stdout.lines() {
        if let Some(rest) = line.strip_prefix("! ") {
            return rest
                .split_whitespace()
                .next()
                .expect("! line without code")
                .to_string();
        }
    }
    panic!(
        "expected error envelope (code={}) — stdout:\n{}\nstderr:\n{}",
        out.code, out.stdout, out.stderr
    );
}

/// Helper struct that bundles a fixture server, a daemon home dir,
/// and a daemon. The whole thing tears down via Drop.
pub struct TestContext {
    pub server: FixtureServer,
    pub home: tempfile::TempDir,
    pub daemon: DaemonGuard,
}

impl TestContext {
    pub fn start() -> Self {
        Self::start_with_env(&[])
    }

    /// Variant of [`Self::start`] that injects extra env vars into
    /// the spawned `vs serve`. Used by capability-gate tests like
    /// `cell_engine_unsupported_when_install_disabled`, which sets
    /// `VS_DISABLE_INSPECTOR=1` so the inspector install path
    /// short-circuits and the daemon advertises the flag as `false`.
    pub fn start_with_env(env: &[(&str, &str)]) -> Self {
        let server = FixtureServer::start();
        let home = tempfile::tempdir().unwrap();
        let key_path = home.path().join("key");
        let key = vs_store::MasterKey::generate().expect("generate master key");
        key.write_to_file(&key_path).expect("write master key");
        let daemon = spawn_daemon_with_env(home.path(), env);
        Self {
            server,
            home,
            daemon,
        }
    }

    pub fn home_path(&self) -> &Path {
        self.home.path()
    }

    pub fn url(&self, path: &str) -> String {
        self.server.url(path)
    }

    pub fn vs(&self, args: &[&str]) -> CliOut {
        vs(self.home.path(), args)
    }
}
