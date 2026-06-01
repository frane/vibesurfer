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
}
