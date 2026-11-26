//! `vs serve` — host the daemon in this process.
//!
//! Folded into the `vs` binary so the project ships exactly one CLI
//! surface; the M0 `vibesurferd` binary is gone. Auto-spawn re-execs
//! `vs serve` instead.
//!
//! # Threading model
//!
//! On **macOS**, `WKWebView` is hard-pinned to the Cocoa main thread.
//! [`run`] therefore stays on the OS main thread, initializes
//! `NSApplication`, constructs the `WkBackend` here, and spawns a
//! worker thread that runs the tokio runtime + the daemon. Engine calls
//! issued by the daemon (on tokio workers) flow through an mpsc
//! channel back to main, where they're drained between NSRunLoop
//! ticks. See [`vs_engine_webkit::runtime::MainThreadDispatcher`].
//!
//! On **Linux**, the same shape applies with a GLib main context and
//! WebKitGTK 6. On **Windows**, with a Win32 message pump and
//! WebView2.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

use anyhow::{Context as _, Result};
use vs_daemon::config::Paths as DaemonPaths;

/// Args specific to `vs serve`. `paths` is the resolved daemon home.
pub struct ServeArgs {
    pub paths: DaemonPaths,
    /// If true, do not start a daemon — instead, read the PID file,
    /// send SIGTERM, and wait for the socket to disappear.
    pub stop: bool,
}

/// `vs serve --stop`. Reads the daemon PID file, sends SIGTERM (Unix)
/// or TerminateProcess (Windows), and waits up to 5s for the socket
/// file to disappear. Stale PID files for processes that already exit
/// are cleaned up.
pub fn run_stop(paths: &DaemonPaths) -> Result<()> {
    let pid_file = paths.pid_file();
    let pid_str = match std::fs::read_to_string(&pid_file) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("no daemon running (no PID file at {})", pid_file.display());
            return Ok(());
        }
        Err(e) => return Err(e).context("read pid file"),
    };
    let pid: i32 = pid_str
        .trim()
        .parse()
        .with_context(|| format!("malformed PID file: {pid_str:?}"))?;
    #[cfg(unix)]
    {
        let r = unsafe { libc::kill(pid, libc::SIGTERM) };
        if r != 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ESRCH) {
                let _ = std::fs::remove_file(&pid_file);
                eprintln!("no daemon running (cleaned stale PID file for pid {pid})");
                return Ok(());
            }
            return Err(anyhow::anyhow!("kill({pid}, SIGTERM) failed: {e}"));
        }
    }
    #[cfg(windows)]
    {
        use ::windows::Win32::Foundation::CloseHandle;
        use ::windows::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_TERMINATE,
        };
        let pid_u32 =
            u32::try_from(pid).with_context(|| format!("PID {pid} out of range for a DWORD"))?;
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, false, pid_u32)
                .map_err(|e| anyhow::anyhow!("OpenProcess({pid}): {e}"))?;
            let r = TerminateProcess(h, 0);
            let _ = CloseHandle(h);
            r.map_err(|e| anyhow::anyhow!("TerminateProcess({pid}): {e}"))?;
        }
    }
    let socket = paths.socket();
    for _ in 0..50 {
        if !socket.exists() {
            eprintln!("daemon stopped (pid {pid})");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(anyhow::anyhow!(
        "daemon (pid {pid}) did not exit within 5s; socket still present at {}",
        socket.display()
    ))
}

/// Hold the daemon's instance lock, a file next to the socket, for as
/// long as the returned handle lives. `None` means another daemon holds
/// it. Parallel callers (test runners start many `vs` at once) each
/// auto-spawn a daemon; without the lock a second one replaced the
/// first one's socket, and clients that had connected in between kept
/// talking to a daemon nobody else could reach.
fn claim_instance(paths: &DaemonPaths) -> Result<Option<std::fs::File>> {
    let path = paths.socket().with_extension("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("lock {}", path.display()))
        }
    }
}

/// [`claim_instance`] for `run`: the lock, or `None` after telling the
/// caller a daemon is already up (an exit that is not an error: the
/// spawning CLI connects to that one).
fn instance_or_exit(paths: &DaemonPaths) -> Result<Option<std::fs::File>> {
    let lock = claim_instance(paths)?;
    if lock.is_none() {
        eprintln!(
            "a vibesurfer daemon is already running for {}",
            paths.socket().display()
        );
    }
    Ok(lock)
}

/// Resolve the daemon master key: OS keyring first, then the fallback
/// file at `~/.vibesurfer/key` (32 raw bytes, 64 hex chars, or base64
/// of 32 bytes). If neither exists, generate a fresh key and persist
/// it to the fallback file (mode 0600) so `vs auth save|load` works
/// out of the box. Returns `None` — with the cause logged — only when
/// the file is present but unusable, or generation/persist fails.
fn resolve_master_key(paths: &DaemonPaths) -> Option<vs_store::MasterKey> {
    use vs_store::MasterKey;
    if let Ok(k) = MasterKey::from_keyring() {
        return Some(k);
    }
    let key_path = paths.key_file();
    if key_path.exists() {
        match MasterKey::from_file(&key_path) {
            Ok(k) => Some(k),
            Err(e) => {
                tracing::warn!(
                    "master key file {} exists but is unusable ({e}); \
                     vs_auth save|load will fail",
                    key_path.display()
                );
                None
            }
        }
    } else {
        match MasterKey::generate().and_then(|k| k.write_to_file(&key_path).map(|()| k)) {
            Ok(k) => {
                tracing::info!(
                    "no master key found; generated one at {} (keyring entry absent)",
                    key_path.display()
                );
                Some(k)
            }
            Err(e) => {
                tracing::warn!(
                    "no master key, and generating one at {} failed ({e}); \
                     vs_auth save|load will fail",
                    key_path.display()
                );
                None
            }
        }
    }
}

/// Future that resolves on SIGTERM (Unix) or never (other platforms).
/// Lets the server loop treat SIGTERM equivalently to ctrl-c.
#[cfg(unix)]
pub async fn wait_terminate() {
    if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        s.recv().await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[cfg(not(unix))]
pub async fn wait_terminate() {
    std::future::pending::<()>().await;
}

#[cfg(target_os = "linux")]
pub use linux::run;
#[cfg(target_os = "macos")]
pub use macos::run;
#[cfg(target_os = "windows")]
pub use windows::run;

fn init_tracing() {
    if tracing::dispatcher::has_been_set() {
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vs_daemon=info,info")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}

/// Install a panic hook that logs the panic via tracing::error
/// before the default hook prints to stderr. The default hook
/// already writes to stderr, but on Windows a panic on a non-main
/// thread can sometimes terminate the process before stderr is
/// flushed; routing through tracing first guarantees the panic
/// reaches the daemon log file the test harness captures.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map_or_else(|| "<unknown>".to_string(), ToString::to_string);
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<no message>");
        tracing::error!(at = %location, "PANIC: {msg}");
        prev(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_daemon_does_not_get_the_instance() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DaemonPaths::at(dir.path());
        let first = claim_instance(&paths).unwrap();
        assert!(first.is_some());
        assert!(claim_instance(&paths).unwrap().is_none());
        drop(first);
        assert!(
            claim_instance(&paths).unwrap().is_some(),
            "free again once the first daemon exits"
        );
    }
}
