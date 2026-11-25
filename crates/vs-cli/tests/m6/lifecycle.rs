//! Lifecycle cells: vs_session_open, vs_session_close, vs_open,
//! vs_close, vs_view, vs_read, vs_status.

use crate::helpers::{eval_js, open_fixture, ref_for, settle};
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

/// A second `vs serve` on a home whose daemon is up leaves it alone.
/// Test runners start many `vs` at once on a fresh home; each one that
/// found no daemon spawned one, and a later daemon replaced the socket
/// of an earlier one, stranding the clients already on it.
#[test]
fn cell_second_daemon_leaves_the_first_alone() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let r = ctx.vs(&["session-open"]);
        assert_ok("session-open", &r);
        let pid_file = ctx.home_path().join("daemon.pid");
        let first_pid = std::fs::read_to_string(&pid_file).unwrap_or_default();
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_vs"))
            .arg(format!("--home={}", ctx.home_path().display()))
            .arg("serve")
            .output()
            .expect("run a second vs serve");
        assert!(
            out.status.success(),
            "second daemon should step aside: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("already running"),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(&pid_file).unwrap_or_default(),
            first_pid,
            "the PID file still names the first daemon"
        );
        let r = ctx.vs(&["open", &ctx.url("/static.html")]);
        assert_ok("open on the first daemon's session", &r);
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

/// The product's 15s load budget must not include web-process launch.
///
/// A cold WKWebView spends the first navigation spawning its
/// web-content process. Charging that against the load budget made
/// the first `vs open` of a daemon return `! TIMEOUT 15000ms open`.
/// The harness raises `VS_NAV_BUDGET_MS` to 45s so the rest of the
/// suite is not measuring process spawn; this cell puts the product
/// default back and still expects a page.
#[test]
fn cell_open_cold_process_is_not_a_load_timeout() {
    for _ in each_available_backend() {
        let ctx = TestContext::start_with_env(&[("VS_NAV_BUDGET_MS", "15000")]);
        let r = ctx.vs(&["open", &ctx.url("/static.html")]);
        assert_ok("open with the 15s load budget", &r);
        assert!(
            !r.stdout.contains("TIMEOUT"),
            "a cold open must not be reported as a load timeout:\n{}",
            r.stdout
        );
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

// A label capped mid-emoji, or a page string with a lone surrogate,
// used to fail the whole view: JSON.stringify escapes the lone half
// and serde_json rejects it. Found on a ScienceDirect article page.
#[test]
fn cell_view_survives_lone_surrogates() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/surrogate.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        assert_ok("view --full", &r);
        let body = body_rest(&r);
        assert!(
            body.contains("Still here") && body.contains("low \u{FFFD} end"),
            "view must render with lone surrogates replaced:\n{body}"
        );
    }
}

// Headings carry level=N and a checked checkbox carries checked=1,
// so a client can filter by heading level and assert checked state
// from the tree alone.
#[test]
fn cell_view_heading_level_and_checked() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/static.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        assert_ok("view --full", &r);
        let body = body_rest(&r);
        let has = |needle: &str| body.lines().any(|l| l.contains(needle));
        assert!(
            body.lines()
                .any(|l| l.contains("hd") && l.contains("Static fixture") && l.contains("level=1")),
            "h1 must carry level=1:\n{body}"
        );
        assert!(
            body.lines()
                .any(|l| l.contains("hd") && l.contains("Forms") && l.contains("level=2")),
            "h2 must carry level=2:\n{body}"
        );
        assert!(!has("checked=1"), "nothing is checked yet:\n{body}");
        // The checkbox's own label is its value ("on"); the wrapping
        // <label> text rides as lbl=.
        let chk_line = body
            .lines()
            .find(|l| l.contains(" chk ") && l.contains("lbl=\"Accept terms\""))
            .unwrap_or_else(|| panic!("checkbox must carry lbl=\"Accept terms\":\n{body}"));
        let chk: u32 = chk_line.split_whitespace().next().unwrap().parse().unwrap();
        let tok = token_of(&r);
        let a = ctx.vs(&[
            "act",
            &page,
            &chk.to_string(),
            "click",
            &format!("--token={tok}"),
        ]);
        assert_ok("act click checkbox", &a);
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        assert!(
            body.lines().any(|l| l.contains("chk")
                && l.contains("Accept terms")
                && l.contains("checked=1")),
            "checked box must carry checked=1:\n{body}"
        );
    }
}

// SIGTERM stops the daemon by itself, with a page open. The reaper
// thread held a strong daemon handle, so the engine channel never
// closed and `vs serve` hung until SIGKILL, orphaning the page's
// web-content process (and every m6 teardown fell back to SIGKILL).
#[cfg(unix)]
#[test]
fn cell_sigterm_stops_the_daemon() {
    for _ in each_available_backend() {
        let mut ctx = TestContext::start();
        let (_s, _page, _t) = open_fixture(&ctx, "/static.html");
        assert!(
            ctx.daemon
                .terminate_within(std::time::Duration::from_secs(10)),
            "vs serve did not exit within 10s of SIGTERM"
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
//
// All three engines since v0.2.6. It was macOS-only before that, and
// the cell was gated to macOS for as long, which is why a primitive
// the docs sell as the fast path for successive navigations went two
// engines without one.
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

/// A page nobody is using loses its web view, and comes back when
/// someone wants it again.
///
/// The daemon is auto-spawned and long-lived, and agents die without
/// calling `session-close` — that is the ordinary way an agent run
/// ends. One host reached 48 sessions and 162 open pages over four
/// days, 173 WebKit processes and ~8.6 GB, and the only thing that
/// ever brought it down was a human noticing the fans.
#[test]
fn cell_idle_pages_are_reaped_and_recreated() {
    for _ in each_available_backend() {
        // Seconds, not the shipped half-hour: same code path, budget a
        // cell can wait for. Long enough that rebuilding the page (3 s
        // on a slow macOS runner, counted from the call's start) does
        // not run past it before the next status.
        let ctx =
            TestContext::start_with_env(&[("VS_PAGE_IDLE_SECS", "5"), ("VS_REAP_SWEEP_SECS", "1")]);
        let (session, page, _t) = open_fixture(&ctx, "/static.html");

        let r = ctx.vs(&["status"]);
        assert_ok("status", &r);
        assert!(
            body_rest(&r).contains("live=1"),
            "a fresh page holds a web view:\n{}",
            body_rest(&r)
        );

        // Wait out the idle window without touching the page.
        settle(7500);
        let r = ctx.vs(&["status"]);
        let body = body_rest(&r);
        assert!(
            body.contains("pages=1") && body.contains("live=0"),
            "the idle page must go dormant, keeping its row:\n{body}"
        );

        // Dormant is not gone: the next call rebuilds the web view and
        // the agent sees the page it left.
        let r = ctx.vs(&["--session", &session, "view", &page, "--full"]);
        assert_ok("view after reap", &r);
        assert!(
            body_rest(&r).contains("Static fixture"),
            "the rebuilt page is the same page:\n{}",
            body_rest(&r)
        );
        let r = ctx.vs(&["status"]);
        assert!(
            body_rest(&r).contains("live=1"),
            "using it brings the web view back:\n{}",
            body_rest(&r)
        );
    }
}

/// `vs session-close --idle-for` sweeps what agents left behind, and
/// spares what is still in use.
#[test]
fn cell_session_close_sweeps_idle_sessions() {
    for _ in each_available_backend() {
        // Reaping off: this cell is about the manual remedy.
        let ctx = TestContext::start_with_env(&[("VS_REAP", "0")]);
        let (stale, _page, _t) = open_fixture(&ctx, "/static.html");
        settle(2500);
        // A second session, touched just now.
        let r = ctx.vs(&["session-open"]);
        assert_ok("session-open", &r);
        let fresh = body_first(&r);

        let r = ctx.vs(&["session-close", "--idle-for=2s"]);
        assert_ok("session-close --idle-for", &r);
        let swept = body_rest(&r);
        assert!(
            swept.contains(&stale),
            "the idle session must be closed:\n{swept}"
        );
        assert!(
            !swept.contains(&fresh),
            "a session touched a moment ago must survive:\n{swept}"
        );

        let r = ctx.vs(&["status"]);
        let body = body_rest(&r);
        assert!(!body.contains(&stale), "closed session is gone:\n{body}");
        assert!(body.contains(&fresh), "live session remains:\n{body}");
    }
}

// `vs init-script` runs at document start of later documents, before
// the page's own scripts, and leaves the current document alone.
#[test]
fn cell_init_script_runs_at_document_start() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/download.html");
        let r = ctx.vs(&[
            "init-script",
            &page,
            "window.__vsInit = (window.__vsInit || 0) + 1; window.__vsInitSawBody = !!document.body;",
        ]);
        assert_ok("init-script", &r);
        assert_eq!(
            eval_js(&ctx, &page, "String(window.__vsInit)"),
            "undefined",
            "the current document must not run the script"
        );
        let r = ctx.vs(&["goto", &page, &ctx.url("/download.html")]);
        assert_ok("goto", &r);
        assert_eq!(eval_js(&ctx, &page, "window.__vsInit"), "1");
        assert_eq!(
            eval_js(&ctx, &page, "window.__vsInitSawBody"),
            "false",
            "the script ran after the document was parsed, not at document start"
        );
    }
}

// A page whose CSP forbids every script still gets the init script.
#[test]
fn cell_init_script_ignores_page_csp() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/download.html");
        let r = ctx.vs(&["init-script", &page, "window.__vsInitCsp = 'ran';"]);
        assert_ok("init-script", &r);
        let r = ctx.vs(&["goto", &page, &ctx.url("/csp.html")]);
        assert_ok("goto csp", &r);
        assert_eq!(
            eval_js(&ctx, &page, "String(window.__pageScriptRan)"),
            "undefined"
        );
        assert_eq!(eval_js(&ctx, &page, "window.__vsInitCsp"), "ran");
    }
}

// A same-origin iframe's content is part of the tree: its elements
// get refs, fill and click reach them (the click lands at the right
// spot despite the frame's offset and border), and a change inside
// the frame shows up in the next view.
#[test]
fn cell_same_origin_frame_is_in_the_tree() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/frames.html");
        settle(500);
        let r = ctx.vs(&["view", &page, "--full"]);
        assert_ok("view", &r);
        let body = body_rest(&r);
        let token = token_of(&r);
        let field = ref_for(&body, "tf", "");
        let button = ref_for(&body, "btn", "Greet from frame");
        let r = ctx.vs(&[
            "act",
            &page,
            &field.to_string(),
            "fill",
            "Ada",
            &format!("--token={token}"),
        ]);
        assert_ok("fill in frame", &r);
        let r = ctx.vs(&["view", &page]);
        let token = token_of(&r);
        let r = ctx.vs(&[
            "act",
            &page,
            &button.to_string(),
            "click",
            &format!("--token={token}"),
        ]);
        assert_ok("click in frame", &r);
        settle(300);
        let r = ctx.vs(&["view", &page, "--full"]);
        assert!(
            r.stdout.contains("Hello Ada"),
            "the frame's change is not in the tree:\n{}",
            r.stdout
        );
    }
}

// document.open() replaces the document but keeps the window. The
// view cache must notice changes in the new document, not keep
// answering with the first walk of it.
#[test]
fn cell_view_tracks_a_rewritten_document() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/download.html");
        eval_js(
            &ctx,
            &page,
            "(document.open(), document.write('<button id=\"b\" onclick=\"this.textContent=\\'done\\'\">Go</button>'), document.close(), 'ok')",
        );
        let r = ctx.vs(&["view", &page, "--full"]);
        assert_ok("view", &r);
        let body = body_rest(&r);
        let token = token_of(&r);
        let b = ref_for(&body, "btn", "Go");
        let r = ctx.vs(&[
            "act",
            &page,
            &b.to_string(),
            "click",
            &format!("--token={token}"),
        ]);
        assert_ok("click", &r);
        settle(200);
        let r = ctx.vs(&["view", &page, "--full"]);
        assert!(
            r.stdout.contains("done"),
            "stale view after document.open:\n{}",
            r.stdout
        );
    }
}
