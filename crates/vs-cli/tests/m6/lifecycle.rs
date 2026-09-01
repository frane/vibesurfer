//! Lifecycle cells: vs_session_open, vs_session_close, vs_open,
//! vs_close, vs_view, vs_read, vs_status.

use crate::helpers::{open_fixture, ref_for};
use crate::support::{
    assert_ok, body_first, body_rest, each_available_backend, token_of, TestContext,
};

// 1. vs_session_open
#[test]
fn cell_session_open() {
    for backend in each_available_backend() {
        eprintln!("== cell_session_open: backend={} ==", backend.name());
        let ctx = TestContext::start();
        let r = ctx.vs(&["session-open"]);
        assert_ok("session-open", &r);
        let session = body_first(&r);
        assert!(session.starts_with("s_"), "session={session:?}");
        let token = token_of(&r);
        assert_eq!(token.len(), 16, "state_token must be 16-hex; got {token:?}");
    }
}

// 2. vs_session_close
#[test]
fn cell_session_close() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let r = ctx.vs(&["session-open"]);
        assert_ok("session-open", &r);
        let r = ctx.vs(&["session-close"]);
        assert_ok("session-close", &r);
        let r = ctx.vs(&["open", &ctx.url("/static.html")]);
        assert_ne!(r.code, 0, "open after close should fail: {}", r.stdout);
    }
}

// 3. vs_open
#[test]
fn cell_open() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        ctx.vs(&["session-open"]);
        let r = ctx.vs(&["open", &ctx.url("/static.html")]);
        assert_ok("open", &r);
        let page = body_first(&r);
        assert!(page.starts_with("p_"), "page={page:?}");
        let token = token_of(&r);
        assert_eq!(token.len(), 16);
    }
}

// 3b. A page-addressed op on a page that lives in a *different* session
// returns WRONG_SESSION (naming the page's real session), not a
// misleading NOT_FOUND. Regression for the #vibesurfer report.
#[test]
fn cell_view_wrong_session() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let a = body_first(&ctx.vs(&["session-open"]));
        let b = body_first(&ctx.vs(&["session-open"]));
        let open = ctx.vs(&["open", &ctx.url("/static.html"), &format!("--session={a}")]);
        assert_ok("open in A", &open);
        let page = body_first(&open);
        // Address the page from the wrong session B.
        let r = ctx.vs(&["view", &page, &format!("--session={b}")]);
        assert!(
            r.stdout.contains("WRONG_SESSION"),
            "page from A viewed in B must be WRONG_SESSION; got {:?}",
            r.stdout
        );
        assert!(
            r.stdout.contains(&a),
            "WRONG_SESSION should name the page's real session A; got {:?}",
            r.stdout
        );
        // Sanity: it works in its own session A.
        assert_ok(
            "view in A",
            &ctx.vs(&["view", &page, &format!("--session={a}")]),
        );
    }
}

// 4. vs_close
#[test]
fn cell_close() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/static.html");
        let r = ctx.vs(&["close", &page]);
        assert_ok("close", &r);
        let r = ctx.vs(&["view", &page]);
        assert_ne!(r.code, 0, "view after close should fail: {}", r.stdout);
    }
}

// 5. vs_view
#[test]
fn cell_view() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/static.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        assert_ok("view --full", &r);
        let body = body_rest(&r);
        assert!(
            body.contains("Static fixture"),
            "view body must contain the rendered h1:\n{body}"
        );
        assert!(body.contains("btn"), "view should contain btn role: {body}");
    }
}

// vs_view must never emit ref 0 for a real node: 0 is the delta
// grammar's ROOT sentinel, so a real node at ref 0 makes +/@ parent
// references ambiguous. A roleless wrapper with >=2 children used to
// leak through as `0 el ""` (reported via #vibesurfer).
#[test]
fn cell_view_never_emits_ref_zero() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/ref-nonzero.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        assert_ok("view --full", &r);
        let body = body_rest(&r);
        for line in body.lines() {
            let first = line.trim_start().split(' ').next().unwrap_or("");
            assert_ne!(
                first, "0",
                "view emitted a node with ref 0 (collides with Ref::ROOT):\n{body}"
            );
        }
        assert!(
            body.contains("Alpha") && body.contains("Bravo"),
            "both wrapper children must still be present:\n{body}"
        );
    }
}

// 6. vs_read
#[test]
fn cell_read() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/static.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let n = ref_for(&body, "btn", "Submit");
        let r = ctx.vs(&["read", &page, &n.to_string()]);
        assert_ok("read", &r);
        assert!(
            r.stdout.contains("Submit"),
            "read should return Submit text:\n{}",
            r.stdout
        );
    }
}

// 28. vs_status (lives here because it's a session-level read).
#[test]
fn cell_status() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (session, page, _t) = open_fixture(&ctx, "/static.html");
        let r = ctx.vs(&["status"]);
        assert_ok("status", &r);
        assert!(
            r.stdout.contains(&session) && r.stdout.contains(&page),
            "status must reference current session + page:\n{}",
            r.stdout
        );
        // The daemon identity line lets clients auto-negotiate flags.
        assert!(
            r.stdout.contains("daemon\tversion=") && r.stdout.contains("flags=actDeltas,clickVia"),
            "status must advertise daemon version + negotiable flags:\n{}",
            r.stdout
        );
    }
}

// vs_goto navigates a page in place: same page id, new document,
// fresh refs. Much cheaper than open (reuses the web view).
// goto is implemented on the Cocoa backend; wpe/webview2 return
// ENGINE_UNSUPPORTED, so this cell is macOS-only for now.
#[cfg(target_os = "macos")]
#[test]
fn cell_goto_navigates_in_place() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (session, page, _t) = open_fixture(&ctx, "/static.html");
        let r = ctx.vs(&["goto", &page, &ctx.url("/form.html")]);
        assert_ok("goto", &r);
        // Same page id comes back.
        assert!(
            r.stdout.contains(&page),
            "goto must return the same page id {page}:\n{}",
            r.stdout
        );
        // The page now shows the form document, and status reflects the
        // new url on the same page.
        let v = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&v);
        assert!(
            body.contains("frm"),
            "after goto to form.html the view should contain a form:\n{body}"
        );
        let st = ctx.vs(&["status"]);
        assert!(
            st.stdout.contains("form.html") && st.stdout.contains(&session),
            "status should show the new url on the same session:\n{}",
            st.stdout
        );
    }
}

// vs flow run executes a JSON step list in one session, threading
//  and  so scripted flows need no manual id/token plumbing.
// This flow exercises goto, which is macOS-only for now (wpe/webview2
// return ENGINE_UNSUPPORTED).
#[cfg(target_os = "macos")]
#[test]
fn cell_flow_run_executes_steps() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let form = ctx.url("/form.html");
        let dash = ctx.url("/dashboard.html");
        let flow = serde_json::json!([
            ["open", form],
            ["view", "$page", "--full"],
            ["goto", "$page", dash],
            ["view", "$page"],
            ["status"],
        ])
        .to_string();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flow.json");
        std::fs::write(&path, flow).unwrap();
        let r = ctx.vs(&["flow", "run", path.to_str().unwrap()]);
        assert_eq!(
            r.code, 0,
            "flow run should succeed:\nstdout={}\nstderr={}",
            r.stdout, r.stderr
        );
        assert!(
            r.stderr.contains("5 step(s) ok"),
            "flow should report all steps ok:\n{}",
            r.stderr
        );
    }
}

// Sessions survive a daemon restart. state.db is the source of truth

// (ARCHITECTURE.md); a restart used to drop every open session and
// strand parked agents in WRONG_SESSION — reported after an x.com
// login was lost mid prompt_form_wait (#vibesurfer 01KXJV).
#[test]
fn cell_session_resurrection_across_daemon_restart() {
    for _ in crate::support::each_available_backend() {
        let server = crate::support::FixtureServer::start();
        let home = tempfile::tempdir().unwrap();
        let key = vs_store::MasterKey::generate().unwrap();
        key.write_to_file(home.path().join("key")).unwrap();

        let daemon = crate::support::spawn_daemon(home.path());
        let open = crate::support::vs(home.path(), &["open", &server.url("/form.html")]);
        crate::support::assert_ok("open before restart", &open);
        let page = open
            .stdout
            .split_whitespace()
            .find(|w| w.starts_with("p_"))
            .expect("page id")
            .to_string();
        let session = crate::support::vs(home.path(), &["status"])
            .stdout
            .split_whitespace()
            .find(|w| w.starts_with("s_"))
            .expect("session id")
            .to_string();

        // Hard restart: kill without session-close, then respawn on
        // the same home.
        drop(daemon);
        std::thread::sleep(std::time::Duration::from_millis(300));
        // No stale-socket cleanup: is_listening is a real connect
        // probe now, so the helper must wait out the leftover file.
        let _daemon2 = crate::support::spawn_daemon(home.path());

        // Same session id, same page id, fully usable.
        let r = crate::support::vs(
            home.path(),
            &["--session", &session, "view", &page, "--full"],
        );
        crate::support::assert_ok("view after restart", &r);
        assert!(
            r.stdout.contains("Sign in"),
            "resurrected page must render its URL again:\n{}",
            r.stdout
        );
    }
}

/// The documented shell flow must survive command substitution.
///
/// `P=$(vs open URL)` runs `vs` inside a subshell, so the caller key
/// used to differ from the one the next `vs view $P` computed — each
/// invocation auto-created its own session and the view came back
/// `! WRONG_SESSION`. That is the most natural way to script this tool
/// and it could not work; it also leaked a session row and a
/// `callers/` file per call.
///
/// Runs the real shell rather than the harness's direct spawn, because
/// the subshell is the whole point.
///
/// Unix only, and not merely because `sh` is: the fix keys on the POSIX
/// session id, which Windows has no equivalent of. Windows still falls
/// back to the parent pid and so still has this bug — `VS_CALLER` is
/// the reliable binding there.
#[cfg(unix)]
#[test]
fn cell_session_survives_command_substitution() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let home = ctx.home_path().display().to_string();
        let vs = env!("CARGO_BIN_EXE_vs");
        let url = ctx.url("/static.html");

        // Each `$( )` is a distinct subshell — the shape that broke.
        let script = format!(
            "P=$({vs} --home={home} --no-spawn open {url} | sed -n '2p'); \
             {vs} --home={home} --no-spawn view \"$P\""
        );
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .env_remove("VS_SESSION")
            .env_remove("VS_CALLER")
            .output()
            .expect("run shell flow");
        let stdout = String::from_utf8_lossy(&out.stdout);

        assert!(
            !stdout.contains("WRONG_SESSION"),
            "a page opened in a subshell must be visible to the next call:\n{stdout}"
        );
        assert!(
            stdout.contains("doc"),
            "expected a tree from view, got:\n{stdout}"
        );
    }
}
