//! Regression tests for the auto-spawn path in `commands::dispatch`.
//!
//! Two bugs landed in `fc99183`:
//!
//! 1. `spawn_daemon(&[])` was called with no extra args — so a CLI
//!    invoked with `--home=<path>` would auto-spawn a daemon at the
//!    *default* `~/.vibesurfer/`, not at `<path>`. The CLI then
//!    waited forever for a socket that would never appear.
//!
//! 2. The `socket.exists()` short-circuit was Unix-only correct;
//!    Windows named pipes don't appear on the filesystem so the
//!    check would always trigger an auto-spawn even when a daemon
//!    was already running.
//!
//! These tests intercept the auto-spawn path by setting
//! `VS_DAEMON_BIN` to a fake binary that records the args it
//! received, so we can pin the contract without booting a real
//! engine.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const VS_BIN: &str = env!("CARGO_BIN_EXE_vs");

const FAKE_VS_SCRIPT: &str = r#"#!/usr/bin/env bash
printf '%s\0' "$@" > "$VS_FAKE_LOG"
"#;

/// Drop a fake `vs` binary into `dir` that just NUL-joins its argv
/// to `$VS_FAKE_LOG` and exits 0. Returns the path to the fake.
fn fake_vs_bin(dir: &Path) -> std::path::PathBuf {
    let bin = dir.join("fake-vs");
    std::fs::write(&bin, FAKE_VS_SCRIPT).unwrap();
    let mut perm = std::fs::metadata(&bin).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&bin, perm).unwrap();
    bin
}

/// Run the real `vs` binary with the auto-spawn path pointed at our
/// fake. Returns the args the fake observed (split on NUL).
fn capture_spawned_args(home: &Path, fake: &Path, argv_log: &Path) -> Vec<String> {
    let _ = std::fs::remove_file(argv_log);
    let out = Command::new(VS_BIN)
        .arg(format!("--home={}", home.display()))
        // No --no-spawn: we *want* the CLI to try auto-spawn so it
        // calls our fake.
        .arg("status")
        .env("VS_DAEMON_BIN", fake)
        .env("VS_FAKE_LOG", argv_log)
        .output()
        .expect("run vs");
    // The CLI fails (no socket bound by the fake), but it's already
    // launched the fake by then. Give the fake a moment to flush.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !argv_log.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    let raw = std::fs::read(argv_log).unwrap_or_else(|e| {
        panic!(
            "fake never wrote argv log: {e}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    raw.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

#[test]
fn auto_spawn_propagates_home_to_serve() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let fake = fake_vs_bin(dir.path());
    let argv_log = dir.path().join("argv.log");

    let args = capture_spawned_args(&home, &fake, &argv_log);

    // Expect at least: `serve --home=<path>`.
    assert_eq!(args.first().map(String::as_str), Some("serve"));
    let home_flag = format!("--home={}", home.display());
    assert!(
        args.iter().any(|a| a == &home_flag),
        "expected spawned args to include {home_flag:?}; got {args:?}",
    );
}

#[test]
fn auto_spawn_skipped_with_no_spawn_flag() {
    // Sanity check: `--no-spawn` must NOT invoke the spawn path. The
    // fake records args only when invoked, so if our flag works, the
    // log file should never appear.
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let fake = fake_vs_bin(dir.path());
    let argv_log = dir.path().join("argv.log");
    let _ = std::fs::remove_file(&argv_log);

    let _ = Command::new(VS_BIN)
        .arg(format!("--home={}", home.display()))
        .arg("--no-spawn")
        .arg("status")
        .env("VS_DAEMON_BIN", &fake)
        .env("VS_FAKE_LOG", &argv_log)
        .output()
        .expect("run vs");

    // Wait briefly to ensure the fake didn't get invoked.
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        !argv_log.exists(),
        "--no-spawn must skip auto-spawn, but the fake was invoked",
    );
}
