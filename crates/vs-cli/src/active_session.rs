//! Read/write the `active-session` pointer file.
//!
//! `~/.vibesurfer/active-session` holds one line: the id of the most
//! recently-opened session. The CLI reads it on startup; `vs
//! session-open` writes it; `vs session-close` clears it. `--session`
//! on any command overrides.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context as _, Result};

/// Read the active session id, or `None` if the file is missing.
pub fn read(path: impl AsRef<Path>) -> Result<Option<String>> {
    let path = path.as_ref();
    match fs::read_to_string(path) {
        Ok(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Write `id` as the active session.
pub fn write(path: impl AsRef<Path>, id: &str) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(path, format!("{id}\n")).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Clear the active session (delete the file). Idempotent.
pub fn clear(path: impl AsRef<Path>) -> Result<()> {
    match fs::remove_file(path.as_ref()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("remove active-session"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("active-session");
        assert!(read(&path).unwrap().is_none());
        write(&path, "s_abc").unwrap();
        assert_eq!(read(&path).unwrap().as_deref(), Some("s_abc"));
        clear(&path).unwrap();
        assert!(read(&path).unwrap().is_none());
        // Idempotent.
        clear(&path).unwrap();
    }

    #[test]
    fn empty_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("active-session");
        std::fs::write(&path, "\n  \n").unwrap();
        assert!(read(&path).unwrap().is_none());
    }
}
