//! End-to-end script: drive the **real `vs` binary** through every
//! primitive, against a daemon spawned from the same binary, then open
//! the resulting SQLite database and assert on its state.
//!
//! This is the M5 exit gate. It validates the full agent path: argv
//! parsing → wire encoding → Unix-socket transport → in-process daemon
//! → store. The in-process variants live in `tests/end_to_end.rs` and
//! `tests/primitives_10_19.rs`; this one uses `Command::new` so an
//! argv-shape regression in the binary surfaces here.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use vs_store::{ActionFilter, AnnotationTarget, Store};

const VS_BIN: &str = env!("CARGO_BIN_EXE_vs");

/// Owns the spawned `vs serve` child process. Killed on drop so a
/// panicking test doesn't leak the daemon.
struct DaemonGuard {
    child: Child,
}
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_daemon(home: &Path) -> DaemonGuard {
    let child = Command::new(VS_BIN)
        .arg(format!("--home={}", home.display()))
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn vs serve");
    let mut guard = DaemonGuard { child };
    let socket = home.join("daemon.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if socket.exists() {
            return guard;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Daemon never opened the socket — kill the child before panicking
    // so the test process does not leave a zombie behind.
    let _ = guard.child.kill();
    let _ = guard.child.wait();
    panic!(
        "daemon socket {} did not appear within 5s",
        socket.display()
    );
}

struct CliOut {
    code: i32,
    stdout: String,
    stderr: String,
}

fn vs(home: &Path, args: &[&str]) -> CliOut {
    let out = Command::new(VS_BIN)
        .arg(format!("--home={}", home.display()))
        .arg("--no-spawn")
        .args(args)
        .output()
        .expect("run vs");
    CliOut {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Pull `<token>` out of the first `@<token>` line in stdout.
fn token_of(out: &CliOut) -> String {
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

/// First line of body — the line after the envelope. Used for
/// session-open / open / read which return a single payload line.
fn body_first(out: &CliOut) -> String {
    let mut lines = out.stdout.lines();
    let _envelope = lines.next();
    match lines.next() {
        Some(line) => line.to_string(),
        None => panic!("empty body in:\n{}", out.stdout),
    }
}

fn assert_ok(label: &str, out: &CliOut) {
    // Exit 0 = clean success, exit 2 = success with warnings. Both are
    // pass states per `docs/PROTOCOL.md`; only exit 1 (error envelope or
    // local error) should fail this assertion.
    assert!(
        out.code == 0 || out.code == 2,
        "{label}: exit={} stdout={:?} stderr={:?}",
        out.code,
        out.stdout,
        out.stderr
    );
}

#[test]
fn e2e_all_primitives_via_binary() {
    let dir = tempfile::tempdir().unwrap();
    let home: PathBuf = dir.path().to_path_buf();

    // 0. Boot the real daemon.
    let daemon = spawn_daemon(&home);

    // 1. session-open.
    let r = vs(&home, &["session-open", "--policy=default"]);
    assert_ok("session-open", &r);
    let session_id = body_first(&r);
    assert!(session_id.starts_with("s_"), "session_id={session_id:?}");

    // 3. open.
    let r = vs(&home, &["open", "https://example.com/login"]);
    assert_ok("open", &r);
    let page_id = body_first(&r);
    assert!(page_id.starts_with("p_"), "page_id={page_id:?}");

    // 5. view (re-baseline → Full).
    let r = vs(&home, &["view", &page_id]);
    assert_ok("view", &r);
    let view_token = token_of(&r);

    // 6. read ref 1.
    let r = vs(&home, &["read", &page_id, "1"]);
    assert_ok("read", &r);
    assert!(
        r.stdout.contains("[1]"),
        "read body should reference [1]: {}",
        r.stdout
    );

    // 7. act ref 4 click — first call, gets recorded.
    let r = vs(
        &home,
        &[
            "act",
            &page_id,
            "4",
            "click",
            &format!("--token={view_token}"),
            "--group=login-flow",
        ],
    );
    assert_ok("act", &r);
    let act_token = token_of(&r);

    // 8. find.
    let r = vs(&home, &["find", "stub"]);
    assert_ok("find", &r);

    // 9. wait — `stable` succeeds immediately on the stub.
    let r = vs(&home, &["wait", &page_id, "stable", "--timeout=200"]);
    assert_ok("wait", &r);

    // 10. extract — schema "list" works on the stub tree.
    let r = vs(
        &home,
        &["extract", &page_id, "list", &format!("--token={act_token}")],
    );
    assert_ok("extract", &r);
    let extract_token = token_of(&r);

    // 11. mark ref 4 as `submit`.
    let r = vs(
        &home,
        &[
            "mark",
            &page_id,
            "4",
            "submit",
            &format!("--token={extract_token}"),
        ],
    );
    assert_ok("mark", &r);

    // 12. annotate the mark.
    let r = vs(
        &home,
        &["annotate", "mark:submit", "purpose", "primary-cta"],
    );
    assert_ok("annotate", &r);

    // 13. status.
    let r = vs(&home, &["status"]);
    assert_ok("status", &r);
    assert!(
        r.stdout.contains(&session_id),
        "status should include session: {}",
        r.stdout
    );

    // 14. log.
    let r = vs(&home, &["log", "--limit=50"]);
    assert_ok("log", &r);

    // 15. skill list.
    let r = vs(&home, &["skill", "list"]);
    assert_ok("skill list", &r);

    // 16. capture (viewport scope).
    let r = vs(&home, &["capture", &page_id]);
    assert_ok("capture", &r);

    // 17. viewport.
    let r = vs(&home, &["viewport", &page_id, "mobile"]);
    assert_ok("viewport", &r);

    // 18. layout.
    let r = vs(&home, &["layout", &page_id, "1", "2", "4"]);
    assert_ok("layout", &r);

    // 19. auth list — works without a master key (returns an empty list).
    let r = vs(&home, &["auth", "list"]);
    assert_ok("auth list", &r);

    // 4. close the page.
    let r = vs(&home, &["close", &page_id]);
    assert_ok("close", &r);

    // 2. close the session.
    let r = vs(&home, &["session-close"]);
    assert_ok("session-close", &r);

    // Stop the daemon, give SQLite WAL a moment to flush, then assert
    // on the persisted state.
    drop(daemon);
    std::thread::sleep(Duration::from_millis(50));
    assert_store_state(&home, &session_id, &page_id);
}

fn assert_store_state(home: &Path, session_id: &str, page_id: &str) {
    let store = Store::open_read_only(home.join("state.db")).expect("open store ro");

    let session = store
        .get_session(session_id)
        .expect("get_session")
        .expect("session row missing");
    assert_eq!(session.id, session_id);
    assert!(
        session.closed_at.is_some(),
        "session should be closed: {session:?}"
    );

    let page = store
        .get_page(page_id)
        .expect("get_page")
        .expect("page row missing");
    assert_eq!(page.id, page_id);
    assert!(page.closed_at.is_some(), "page should be closed: {page:?}");

    let mark = store
        .get_mark(session_id, "submit")
        .expect("get_mark")
        .expect("mark missing");
    assert_eq!(mark.name, "submit");
    assert_eq!(mark.page_id, page_id);

    let target = AnnotationTarget::Mark("submit".into());
    let annotations = store.list_annotations(&target).expect("list_annotations");
    assert!(
        annotations.iter().any(|a| a.key == "purpose"),
        "annotation missing: {annotations:?}"
    );

    let actions = store
        .list_actions(&ActionFilter {
            session_id: Some(session_id.to_string()),
            ..Default::default()
        })
        .expect("list_actions");
    let primitives: std::collections::HashSet<&str> =
        actions.iter().map(|a| a.primitive.as_str()).collect();
    for p in [
        "vs_session_open",
        "vs_open",
        "vs_view",
        "vs_read",
        "vs_act",
        "vs_find",
        "vs_wait",
        "vs_extract",
        "vs_mark",
        "vs_annotate",
        "vs_status",
        "vs_log",
        "vs_skill",
        "vs_capture",
        "vs_viewport",
        "vs_layout",
        "vs_auth",
        "vs_close",
        "vs_session_close",
    ] {
        assert!(
            primitives.contains(p),
            "missing audit row for {p}: have {primitives:?}"
        );
    }

    let grouped = store
        .list_actions(&ActionFilter {
            session_id: Some(session_id.to_string()),
            group_label: Some("login-flow".into()),
            ..Default::default()
        })
        .expect("list_actions grouped");
    assert!(
        grouped.iter().any(|a| a.primitive == "vs_act"),
        "expected vs_act with group=login-flow: {grouped:?}"
    );

    let captures = home.join("captures");
    assert!(
        captures.is_dir(),
        "captures dir missing: {}",
        captures.display()
    );
    let png_count = std::fs::read_dir(&captures)
        .expect("read captures")
        .filter_map(Result::ok)
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("png"))
        })
        .count();
    assert!(png_count >= 1, "expected ≥1 capture PNG, got {png_count}");
}
