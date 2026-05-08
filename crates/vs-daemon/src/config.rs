//! Filesystem layout: where the daemon keeps state on disk.
//!
//! ```text
//! ~/.vibesurfer/
//!   daemon.sock           # the IPC socket
//!   active-session        # one-line session id
//!   state.db              # SQLite, WAL mode
//!   key                   # AES-256 fallback if OS keyring unavailable
//!   log/                  # rotating tracing output (M5+)
//!   captures/             # screenshot output paths (M5+)
//! ```

use std::path::{Path, PathBuf};

/// Daemon-side filesystem layout.
#[derive(Debug, Clone)]
pub struct Paths {
    /// Root directory; usually `$HOME/.vibesurfer`.
    pub root: PathBuf,
}

impl Paths {
    /// Construct from a custom root (tests use a temp dir).
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Conventional location: `$HOME/.vibesurfer`. Falls back to the
    /// current directory if `$HOME` is unset (rare; CI containers).
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
    pub fn db(&self) -> PathBuf {
        self.root.join("state.db")
    }

    #[must_use]
    pub fn active_session(&self) -> PathBuf {
        self.root.join("active-session")
    }

    #[must_use]
    pub fn key_file(&self) -> PathBuf {
        self.root.join("key")
    }

    #[must_use]
    pub fn captures(&self) -> PathBuf {
        self.root.join("captures")
    }

    /// Ensure the root directory exists.
    pub fn ensure_root(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)
    }
}
