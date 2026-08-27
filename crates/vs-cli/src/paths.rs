//! Filesystem layout known to the CLI.
//!
//! Mirrors `vs_daemon::config::Paths` deliberately rather than pulling
//! the daemon crate as a dependency — the CLI shouldn't compile the
//! daemon's engine + store stack just to know where its socket lives.

use std::path::{Path, PathBuf};

/// Daemon-side filesystem layout, from the CLI's perspective.
#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Conventional location: `$HOME/.vibesurfer`. Falls back to `.` if
    /// `$HOME` is unset.
    #[must_use]
    pub fn home() -> Self {
        let root = std::env::var_os("HOME").map_or_else(
            || PathBuf::from(".vibesurfer"),
            |h| Path::new(&h).join(".vibesurfer"),
        );
        Self::at(root)
    }

    #[must_use]
    pub fn socket(&self) -> PathBuf {
        self.root.join("daemon.sock")
    }

    #[must_use]
    pub fn active_session(&self) -> PathBuf {
        self.root.join("active-session")
    }

    /// Where the session id is stored for a given caller key. The
    /// directory is created lazily on first write so a read-only
    /// `vs status` doesn't side-effect.
    #[must_use]
    pub fn caller_session(&self, caller_key: &str) -> PathBuf {
        self.root.join("callers").join(caller_key)
    }

    /// Where the daemon writes screenshot PNGs. Mirrors
    /// `vs_daemon::config::Paths::captures` — defaults to
    /// `<root>/captures`, overridable via `VS_CAPTURES_DIR` — so
    /// `vs capture clean` prunes exactly what the daemon wrote.
    #[must_use]
    pub fn captures(&self) -> PathBuf {
        if let Some(p) = std::env::var_os("VS_CAPTURES_DIR") {
            return PathBuf::from(p);
        }
        self.root.join("captures")
    }

    /// Where the daemon writes files pulled out of a page by
    /// `vs download`. Mirrors `vs_daemon::config::Paths::downloads`.
    #[must_use]
    pub fn downloads(&self) -> PathBuf {
        if let Some(p) = std::env::var_os("VS_DOWNLOADS_DIR") {
            return PathBuf::from(p);
        }
        self.root.join("downloads")
    }
}
