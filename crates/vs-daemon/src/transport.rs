//! Cross-platform local-IPC name resolution.
//!
//! On Unix the daemon's IPC primitive is an AF_UNIX socket — the
//! caller's `Path` is the socket file. On Windows it's a named pipe;
//! pipes don't live on the filesystem, so we derive a stable
//! namespaced name from the path's bytes (a short hash of the
//! absolute path). Same `Path` from the same caller always resolves
//! to the same pipe; different paths get different pipes.

use std::path::Path;

use interprocess::local_socket::Name;

/// Convert a filesystem `path` into the platform's local-socket name.
#[cfg(unix)]
pub fn path_to_name(path: &Path) -> std::io::Result<Name<'static>> {
    use interprocess::local_socket::{GenericFilePath, ToFsName};
    let p = path.to_path_buf();
    p.to_fs_name::<GenericFilePath>()
}

#[cfg(windows)]
pub fn path_to_name(path: &Path) -> std::io::Result<Name<'static>> {
    use interprocess::local_socket::{GenericNamespaced, ToNsName};
    let key = path_to_pipe_key(path);
    key.to_ns_name::<GenericNamespaced>()
}

#[cfg(windows)]
fn path_to_pipe_key(path: &Path) -> String {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let bytes = abs.to_string_lossy().into_owned().into_bytes();
    let h = blake3::hash(&bytes);
    format!("vibesurfer-{}", &h.to_hex().as_str()[..16])
}

/// True if a listener could plausibly be reached at `path` —
/// existence check on Unix (the socket file is on disk),
/// connect-probe on Windows (named pipes don't appear on the FS).
#[must_use]
pub fn is_listening(path: &Path) -> bool {
    #[cfg(unix)]
    {
        path.exists()
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::prelude::*;
        let Ok(name) = path_to_name(path) else {
            return false;
        };
        interprocess::local_socket::Stream::connect(name).is_ok()
    }
}
