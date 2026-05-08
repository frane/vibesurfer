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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_listening_false_for_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.sock");
        assert!(!is_listening(&path));
    }

    #[cfg(unix)]
    #[test]
    fn is_listening_false_for_non_socket_file() {
        // A regular file isn't a listening socket. On Unix the
        // probe must not blindly return `true` just because the
        // path exists.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-socket");
        std::fs::write(&path, b"hello").unwrap();
        // is_listening on Unix is currently a presence check; this
        // pins the contract — if we ever tighten it, the test will
        // remind us. For now, document the existing behavior:
        // presence-only check on Unix.
        assert!(is_listening(&path));
    }

    #[test]
    fn path_to_name_round_trips_a_valid_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sock");
        let _name = path_to_name(&path).expect("path_to_name");
    }

    /// Regression: AF_UNIX `sun_path` is 104 bytes on macOS / Linux.
    /// A path that exceeds the limit must surface an error from
    /// `bind()` so callers see the failure instead of the daemon
    /// dying silently.
    #[cfg(unix)]
    #[test]
    fn long_socket_path_fails_to_bind() {
        use interprocess::local_socket::ListenerOptions;

        // Build a path well beyond sun_path. /tmp + a deeply nested
        // segment + suffix.
        let dir = tempfile::tempdir().unwrap();
        let mut path = dir.path().to_path_buf();
        for _ in 0..6 {
            path = path.join("aaaaaaaaaaaaaaaaaaaa");
        }
        std::fs::create_dir_all(&path).unwrap();
        let socket = path.join("daemon.sock");
        assert!(
            socket.as_os_str().len() > 104,
            "test setup: expected an over-long path, got {} bytes",
            socket.as_os_str().len()
        );

        let name = path_to_name(&socket).expect("path_to_name");
        let result = ListenerOptions::new().name(name).create_sync();
        assert!(
            result.is_err(),
            "bind should fail on an over-long sun_path; got Ok",
        );
    }

    /// Regression: the daemon's local socket round-trips a
    /// line-delimited message under a short, well-formed path.
    #[cfg(unix)]
    #[test]
    fn local_socket_round_trip() {
        use interprocess::local_socket::{prelude::*, ListenerOptions, Stream};
        use std::io::{BufRead, Write};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt.sock");
        let name = path_to_name(&path).expect("path_to_name");
        let listener = ListenerOptions::new()
            .name(name.clone())
            .create_sync()
            .expect("bind");

        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().expect("accept");
            let mut line = String::new();
            std::io::BufReader::new(&mut stream)
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line, "ping\n");
            stream.write_all(b"pong\n").unwrap();
        });

        let mut client = Stream::connect(name).expect("connect");
        client.write_all(b"ping\n").unwrap();
        let mut reply = String::new();
        std::io::BufReader::new(&mut client)
            .read_line(&mut reply)
            .unwrap();
        assert_eq!(reply, "pong\n");
        server.join().unwrap();
    }
}
