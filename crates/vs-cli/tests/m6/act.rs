//! Act cells: vs_act click / fill / scroll / key / submit / hover / focus.

use crate::helpers::{eval_js, open_fixture, ref_for, settle};
use crate::support::{
    assert_ok, body_first, body_rest, each_available_backend, token_of, TestContext,
};

// 7. vs_act click — click submit, navigate to dashboard.
#[test]
fn cell_act_click() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/form.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n_email = ref_for(&body, "tf", "");
        let r_fill = ctx.vs(&[
            "act",
            &page,
            &n_email.to_string(),
            "fill",
            "user@example.com",
            &format!("--token={token}"),
        ]);
        assert_ok("fill email", &r_fill);
        let _ = token_of(&r_fill);
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n_submit = ref_for(&body, "btn", "Sign in");
        let r_click = ctx.vs(&[
            "act",
            &page,
            &n_submit.to_string(),
            "click",
            &format!("--token={token}"),
        ]);
        assert_ok("click submit", &r_click);
        // The click triggers a real navigation to the dashboard.
        // `stable` (DOM-quiet) can fire on the still-current login
        // page before the navigation commits — observed as a flake on
        // macOS CI — so wait for the dashboard's own text instead.
        let r_wait = ctx.vs(&["wait", &page, "text", "Dashboard", "--timeout=5000"]);
        assert_ok("wait for dashboard text", &r_wait);
        let title = eval_js(&ctx, &page, "document.title");
        assert!(
            title.contains("Dashboard"),
            "after click on submit, document.title should be Dashboard, got {title:?}"
        );
    }
}

// 8. vs_act fill
#[test]
fn cell_act_fill() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/form.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n_email = ref_for(&body, "tf", "");
        let r = ctx.vs(&[
            "act",
            &page,
            &n_email.to_string(),
            "fill",
            "alice@example.com",
            &format!("--token={token}"),
        ]);
        assert_ok("act fill", &r);
        let value = eval_js(&ctx, &page, "document.getElementById('email').value");
        assert!(
            value.contains("alice@example.com"),
            "fill must update value; got {value:?}"
        );
    }
}

// 8b. vs_act fill on a <select> matches an option by its visible label
// (or value), sets it via the native setter, and dispatches a bubbling
// change so React onChange fires. Regression for the #vibesurfer report.
#[test]
fn cell_act_fill_select() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/select.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n = ref_for(&body, "sel", "");
        // Fill by the visible option LABEL, not its `value`.
        let r = ctx.vs(&[
            "act",
            &page,
            &n.to_string(),
            "fill",
            "SDK / npm Package",
            &format!("--token={token}"),
        ]);
        assert_ok("fill select by label", &r);
        let value = eval_js(&ctx, &page, "document.getElementById('s').value");
        assert!(
            value.contains("sdk"),
            "select value should be the matched option's `value`; got {value:?}"
        );
        let log = eval_js(&ctx, &page, "document.getElementById('log').textContent");
        assert!(
            log.contains("changed:sdk"),
            "a native change event must fire (React onChange path); got {log:?}"
        );
    }
}

// 9. vs_act scroll
#[test]
fn cell_act_scroll() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/static.html");
        let _ = eval_js(
            &ctx,
            &page,
            "(()=>{window.__scrolled=false; document.addEventListener('scroll', ()=>{window.__scrolled=true;}, true); document.documentElement.style.minHeight='3000px'; return 'ok';})()",
        );
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n_footer = ref_for(&body, "p", "Footer");
        let r = ctx.vs(&[
            "act",
            &page,
            &n_footer.to_string(),
            "scroll",
            &format!("--token={token}"),
        ]);
        assert_ok("act scroll", &r);
        let scrolled = eval_js(&ctx, &page, "window.__scrolled");
        let scrolly = eval_js(&ctx, &page, "window.scrollY");
        let parsed: f64 = scrolly.trim().parse().unwrap_or(0.0);
        assert!(
            parsed > 0.0 || scrolled.contains("true"),
            "scroll should produce scrollY>0 or a scroll event; scrollY={scrolly:?} listener={scrolled:?}"
        );
    }
}

// 10. vs_act key
#[test]
fn cell_act_key() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/form.html");
        let _ = eval_js(
            &ctx,
            &page,
            "(()=>{window.__lastKey=null; document.addEventListener('keydown', e=>{window.__lastKey=e.key;}); return 'ok';})()",
        );
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n_email = ref_for(&body, "tf", "");
        let r = ctx.vs(&[
            "act",
            &page,
            &n_email.to_string(),
            "key",
            "Enter",
            &format!("--token={token}"),
        ]);
        assert_ok("act key", &r);
        let last = eval_js(&ctx, &page, "window.__lastKey");
        assert!(
            last.contains("Enter"),
            "key Enter should be recorded by handler; got {last:?}"
        );
    }
}

// 11. vs_act submit
#[test]
fn cell_act_submit() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/form.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n_email = ref_for(&body, "tf", "");
        let r = ctx.vs(&[
            "act",
            &page,
            &n_email.to_string(),
            "fill",
            "u@example.com",
            &format!("--token={token}"),
        ]);
        let token = token_of(&r);
        let r = ctx.vs(&[
            "act",
            &page,
            &n_email.to_string(),
            "submit",
            &format!("--token={token}"),
        ]);
        assert_ok("act submit", &r);
        // What this cell is about is whether submit navigates, not how
        // quickly. A single settle followed by one read asserted the
        // speed too, and failed a macOS CI run on a navigation that
        // was merely slow under load. Poll to a deadline instead.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let title = loop {
            let t = eval_js(&ctx, &page, "document.title");
            if t.contains("Dashboard") || std::time::Instant::now() > deadline {
                break t;
            }
            let _ = ctx.vs(&["wait", &page, "stable", "--timeout=500"]);
        };
        assert!(
            title.contains("Dashboard"),
            "submit should navigate to Dashboard; got {title:?}"
        );
    }
}

// 12. vs_act hover
#[test]
fn cell_act_hover() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/static.html");
        let _ = eval_js(
            &ctx,
            &page,
            "(()=>{window.__hoverEl=null; document.addEventListener('mouseenter', e=>{window.__hoverEl=e.target.tagName;}, true); return 'ok';})()",
        );
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n = ref_for(&body, "btn", "Submit");
        let r = ctx.vs(&[
            "act",
            &page,
            &n.to_string(),
            "hover",
            &format!("--token={token}"),
        ]);
        assert_ok("act hover", &r);
        let last = eval_js(&ctx, &page, "window.__hoverEl");
        assert!(
            last.contains("BUTTON"),
            "hover on button should fire mouseenter; got {last:?}"
        );
    }
}

// 13. vs_act focus
#[test]
fn cell_act_focus() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/form.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n = ref_for(&body, "tf", "");
        let r = ctx.vs(&[
            "act",
            &page,
            &n.to_string(),
            "focus",
            &format!("--token={token}"),
        ]);
        assert_ok("act focus", &r);
        let active_id = eval_js(&ctx, &page, "document.activeElement.id");
        assert!(
            active_id.contains("email"),
            "focus on email field should set activeElement; got {active_id:?}"
        );
    }
}

// vs_act click must emit a full pointer/mouse sequence, not just a bare
// `click`. Regression for the #vibesurfer Radix Select wedge: libraries
// that select on `pointerup` and dismiss on `pointerdown` (Radix's
// dismissable layer) need the real sequence, or the popover never closes
// and its overlay swallows every later click. Runs on all backends — the
// macOS native NSEvent path and the JS path must both deliver it.
#[test]
fn cell_act_click_fires_pointer_sequence() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/click-sequence.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n = ref_for(&body, "btn", "Click me");
        let r = ctx.vs(&[
            "act",
            &page,
            &n.to_string(),
            "click",
            &format!("--token={token}"),
        ]);
        assert_ok("click sequence button", &r);
        let seq = eval_js(&ctx, &page, "window.__seq.join(',')");
        for ev in ["pointerdown", "pointerup", "click"] {
            assert!(
                seq.contains(ev),
                "act click must fire `{ev}` (so Radix-style pointer handlers work); got {seq:?}"
            );
        }
    }
}

// vs_act drains rAF-deferred teardown after the action. A click that
// sets a body pointer-events lock and releases it in a requestAnimationFrame
// (the Radix/Floating-UI dismiss pattern) must not wedge the page —
// critical on headless macOS where the frame clock is paused, so the
// release rAF would otherwise never run. Runs on all backends (macOS via
// the rAF-flush shim; Linux/Windows via native rAF).
#[test]
fn cell_act_flushes_raf_teardown() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/raf-teardown.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n = ref_for(&body, "btn", "go");
        assert_ok(
            "click",
            &ctx.vs(&[
                "act",
                &page,
                &n.to_string(),
                "click",
                &format!("--token={token}"),
            ]),
        );
        settle(150);
        let pe = eval_js(&ctx, &page, "document.body.style.pointerEvents");
        assert!(
            pe.trim().is_empty(),
            "rAF-deferred teardown must have run (body pointer-events cleared); got {pe:?}"
        );
    }
}

// Trust-bit regression — pinned to macOS for now (Linux + Windows
// keep the JS-driven act path until their native event-injection
// lands). The fixture's `submit` button records `event.isTrusted`
// on click; if anyone reverts the WkBackend's NSEvent dispatch back
// to `el.click()`, the assertion flips to `false` and the test
// fails before users hit captcha walls.
#[cfg(target_os = "macos")]
#[test]
fn cell_act_click_is_trusted() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        // Dedicated fixture: button has type="button" so clicking
        // doesn't navigate; the recorded `event.isTrusted` survives
        // for the eval below.
        let (_s, page, _t) = open_fixture(&ctx, "/click-trust.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n = ref_for(&body, "btn", "Click me");
        let r = ctx.vs(&[
            "act",
            &page,
            &n.to_string(),
            "click",
            &format!("--token={token}"),
        ]);
        assert_ok("click trust button", &r);

        let trusted = eval_js(&ctx, &page, "String(window.__vsLastClickTrusted)");
        assert_eq!(
            trusted.trim(),
            "true",
            "vs act click must produce isTrusted=true on macOS; got {trusted:?}",
        );
    }
}

// vs_act fill must work against React-style controlled inputs whose
// per-instance value setter swallows plain `el.value = X` assignments.
// Fixture mimics React's tracker; fill must reach the prototype setter
// so the form submission goes through. v0.1.3 regression.
#[test]
fn cell_act_fill_react_controlled_input() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/react-form.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n_email = ref_for(&body, "tf", "");
        let r_fill = ctx.vs(&[
            "act",
            &page,
            &n_email.to_string(),
            "fill",
            "alice@example.com",
            &format!("--token={token}"),
        ]);
        assert_ok("fill react-controlled input", &r_fill);
        // After fill, the react-style tracker should have observed the
        // new value and updated `state.email`. Probe both to localize
        // a failure: if input.value is empty the prototype setter
        // didn't run; if state.email is empty the input event was
        // either not dispatched or React's deduplication swallowed it.
        let post_fill_value = eval_js(&ctx, &page, "document.getElementById('email').value");
        let post_fill_state = eval_js(
            &ctx,
            &page,
            "(typeof window.__probe_state === 'object' ? window.__probe_state.email : 'unset')",
        );
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n_submit = ref_for(&body, "btn", "Submit");
        let r_click = ctx.vs(&[
            "act",
            &page,
            &n_submit.to_string(),
            "click",
            &format!("--token={token}"),
        ]);
        assert_ok("click submit on react form", &r_click);
        let _ = ctx.vs(&["wait", &page, "stable", "--timeout=2000"]);
        let result = eval_js(&ctx, &page, "document.getElementById('result').textContent");
        assert!(
            result.contains("ok:alice@example.com"),
            "react-style fill must update internal state so submit posts the value; \
             input.value={post_fill_value:?} state.email={post_fill_state:?} result={result:?}"
        );
    }
}

// vs_act click on a zero-size element still reaches its handler. A
// visually-hidden 0x0 button (x.com keeps such duplicates in the DOM)
// can't be hit by native coordinate clicks — the macOS path used to
// ACK without effect; it now falls back to the JS event path.
// Reported via #vibesurfer (01KXJV).
#[test]
fn cell_act_click_zero_size_target() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/zero-size.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let ghost = ref_for(&body, "btn", "Continue");
        // The walker marks the 0x0 button so agents can tell dead
        // duplicates from live ones.
        let ghost_line = body
            .lines()
            .find(|l| l.trim_start().starts_with(&format!("{ghost} ")))
            .unwrap_or_default()
            .to_string();
        assert!(
            ghost_line.contains("hid=1"),
            "zero-size interactive ref must carry hid=1: {ghost_line:?}"
        );
        let r = ctx.vs(&[
            "act",
            &page,
            &ghost.to_string(),
            "click",
            &format!("--token={token}"),
        ]);
        assert_ok("click zero-size button", &r);
        assert!(
            r.stdout.contains("hidden_target"),
            "act on a hidden ref must warn: {:?}",
            r.stdout
        );
        let status = eval_js(&ctx, &page, "document.getElementById('status').textContent");
        assert!(
            status.contains("ghost clicked"),
            "zero-size click must reach the handler, got {status:?}"
        );
    }
}

// vs_type sends trusted keystrokes into the focused element. The
// fixture's mirror records ONLY isTrusted beforeinput data — the
// same gate DraftJS/contenteditable editors use — so a pass proves
// real key events, not a programmatic value write. Requested from an
// x.com composer flow via #vibesurfer (01KXN5).
//
// All three engines now: macOS native NSEvent, Linux XTest (or the
// RemoteDesktop portal under Wayland), Windows through the DevTools
// protocol. The cell is the thing that keeps "verified on all three"
// honest, so it is deliberately not platform-gated.
#[test]
fn cell_type_trusted_into_contenteditable() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/contenteditable.html");
        // Place the caret by focusing the editor (a real flow uses
        // vs_click_at; focus() is enough to route keystrokes here).
        let _ = eval_js(&ctx, &page, "document.getElementById('editor').focus(); 0");
        let r = ctx.vs(&["type", &page, "hi there", "--mode=robotic"]);
        assert_ok("vs type", &r);
        // Key-event delivery to the web-content process is async and
        // load-dependent (CI mac occasionally dropped the last char or
        // two on a single read); poll until the trusted-only mirror
        // reflects the full text, with a deadline.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mirror = loop {
            let m = eval_js(&ctx, &page, "document.getElementById('mirror').textContent");
            if m.contains("hi there") || std::time::Instant::now() > deadline {
                break m;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        assert!(
            mirror.contains("hi there"),
            "trusted-only mirror must capture typed text, got {mirror:?}"
        );
        let kd = eval_js(&ctx, &page, "document.getElementById('kd').textContent");
        assert!(
            kd.trim().parse::<u32>().unwrap_or(0) >= 8,
            "each char must fire a trusted keydown, got {kd:?}"
        );
    }
}

// The robotic click fast path (reduced settle, no path synthesis)
// still registers a real trusted click. Guards the perf change from
// making clicks so fast they do not deliver. macOS native path only.
#[cfg(target_os = "macos")]
#[test]
fn cell_act_click_robotic_still_trusted() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/click-trust.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n = ref_for(&body, "btn", "Click me");
        let r = ctx.vs(&[
            "act",
            &page,
            &n.to_string(),
            "click",
            "--mode=robotic",
            &format!("--token={token}"),
        ]);
        assert_ok("robotic click", &r);
        let trusted = eval_js(&ctx, &page, "String(window.__vsLastClickTrusted)");
        assert_eq!(
            trusted.trim(),
            "true",
            "robotic fast-path click must still register and be trusted; got {trusted:?}",
        );
    }
}

// A trusted drag over a `draggable` element must not start a system
// drag session. On macOS 27 it did: WebKit handed the drag to the
// window server, the daemon's main thread wedged waiting for a
// hardware mouse-up, and a drag window showed on the user's screen
// (#vibesurfer 01M75J1S / 01M75X). On Linux GTK's drag began after
// the drag's release and took the next release on the display, so
// another page's click was lost. The drag must return, the page must
// see the drop, and this page and others must take input afterwards.
#[test]
fn cell_drag_native_draggable_does_not_wedge() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (s, page, token) = open_fixture(&ctx, "/native-drag.html");
        let r = ctx.vs(&["open", &ctx.url("/form.html"), "--session", &s]);
        assert_ok("open second", &r);
        let other = body_first(&r);
        eval_js(
            &ctx,
            &other,
            "(function(){document.body.innerHTML='<input type=checkbox id=c style=\"position:fixed;left:40px;top:40px;width:20px;height:20px\">';return 'ok';})()",
        );
        // Run the drag off-thread: a regression hangs the CLI call
        // forever, and the test should fail, not hang the suite.
        let home = ctx.home_path().to_path_buf();
        let p = page.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let out = crate::support::vs(
                &home,
                &[
                    "drag",
                    &p,
                    "100",
                    "100",
                    "450",
                    "100",
                    &format!("--token={token}"),
                ],
            );
            let _ = tx.send(out);
        });
        let Ok(r) = rx.recv_timeout(std::time::Duration::from_secs(30)) else {
            let log =
                std::fs::read_to_string(ctx.home_path().join("daemon.log")).unwrap_or_default();
            panic!("vs drag over a draggable element must not hang\n--- daemon.log ---\n{log}");
        };
        assert_ok("drag", &r);
        let token = token_of(&r);
        let r = ctx.vs(&["click-at", &page, "100", "320", &format!("--token={token}")]);
        assert_ok("click after drag", &r);
        let log = eval_js(&ctx, &page, "window.__vsLog.join(',')");
        assert!(
            log.contains("drop"),
            "drop target must see the drop, got {log:?}"
        );
        assert!(
            log.contains("click-trusted"),
            "page must take trusted input after the drag, got {log:?}"
        );
        // Another page: a checkbox clicked twice is unchecked again.
        let r = ctx.vs(&["view", &other, "--full"]);
        let mut token = token_of(&r);
        for _ in 0..2 {
            let r = ctx.vs(&[
                "click-at",
                &other,
                "50",
                "50",
                "-M",
                "robotic",
                &format!("--token={token}"),
            ]);
            assert_ok("click-at other page", &r);
            token = token_of(&r);
        }
        settle(200);
        assert_eq!(
            eval_js(&ctx, &other, "String(document.getElementById('c').checked)").trim(),
            "false",
            "both clicks on the other page must land"
        );
    }
}

// A target="_blank" link, a target="_blank" POST form, and
// window.open() all used to go nowhere: the headless page has no
// second window, WebKit dropped the navigation, and the click looked
// like a no-op. ScienceDirect's institution login is a blank-target
// form, so agents could not sign in through it. The navigation now
// loads in the same page, with a POST body intact on the WebKit
// engines. WebView2 hands over only the URL, so Windows skips the POST
// case (docs/known-issues.md).
#[test]
fn cell_new_window_navigation_loads_in_place() {
    for _ in each_available_backend() {
        let cases: [(&str, &str, &str, &str); 3] = [
            (
                "lnk",
                "New-window link",
                "/multi-page-2.html",
                "Multi-page B",
            ),
            ("btn", "New-window POST", "/echo", "entity=escp-idp"),
            ("btn", "Window open", "/multi-page-2.html", "Multi-page B"),
        ];
        for (role, label, path, text) in cases {
            if cfg!(target_os = "windows") && label == "New-window POST" {
                continue;
            }
            let ctx = TestContext::start();
            let (_s, page, _t) = open_fixture(&ctx, "/popup.html");
            let r = ctx.vs(&["view", &page, "--full"]);
            assert_ok("view", &r);
            let target = ref_for(&body_rest(&r), role, label);
            let r = ctx.vs(&[
                "act",
                &page,
                &target.to_string(),
                "click",
                &format!("--token={}", token_of(&r)),
            ]);
            assert_ok(label, &r);
            settle(1500);
            let got_path = eval_js(&ctx, &page, "location.pathname");
            assert!(
                got_path.contains(path),
                "{label}: page must navigate to {path}, got {got_path:?}"
            );
            let body = eval_js(&ctx, &page, "document.body.innerText");
            assert!(
                body.contains(text),
                "{label}: expected {text:?} in body, got {body:?}"
            );
        }
    }
}

// `vs move-to` / `vs hover-at` dispatch trusted mouse moves. On macOS
// they used to reach WKWebView and change nothing: no mouseover, no
// :hover. Menus, tooltips and hover cards that open on a real pointer
// never opened.
#[test]
fn cell_move_to_hovers_with_trusted_events() {
    for _ in each_available_backend() {
        for verb in ["move-to", "hover-at"] {
            let ctx = TestContext::start();
            let (_s, page, _t) = open_fixture(&ctx, "/hover.html");
            let r = ctx.vs(&[verb, &page, "160", "120", "--mode=robotic"]);
            assert_ok(verb, &r);
            settle(100);
            let hovered = eval_js(
                &ctx,
                &page,
                "String(document.querySelector('button:hover') && document.querySelector('button:hover').id)",
            );
            assert_eq!(hovered.trim(), "a", "{verb}: button must match :hover");
            let log = eval_js(&ctx, &page, "window.__vsLog.join(',')");
            for ev in [
                "mouseover-trusted",
                "mouseenter-trusted",
                "mousemove-trusted",
            ] {
                assert!(log.contains(ev), "{verb}: missing {ev}, got {log:?}");
            }
        }
    }
}

// Refs live in the walker's maps, not in DOM attributes: after a view
// and an act, the page's markup is untouched, and the ref still
// resolves to the element.
#[test]
fn cell_refs_stay_out_of_the_markup() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/form.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n_email = ref_for(&body, "tf", "");
        let r_fill = ctx.vs(&[
            "act",
            &page,
            &n_email.to_string(),
            "fill",
            "user@example.com",
            &format!("--token={token}"),
        ]);
        assert_ok("fill email", &r_fill);
        assert_eq!(
            eval_js(
                &ctx,
                &page,
                "String(document.documentElement.outerHTML.includes('data-vs-ref'))"
            ),
            "false",
            "refs leaked into the page's markup"
        );
        assert_eq!(
            eval_js(
                &ctx,
                &page,
                &format!("String(window.__vsFindRef({n_email}).value)")
            ),
            "user@example.com"
        );
    }
}

// Each page has its own pointer, as each Playwright page does: a page
// opens unhovered, and another page's input does not take its hover
// away. On Linux the X pointer and window stacking are shared, so page
// windows must not overlap (each gets a screen tile).
#[test]
fn cell_pages_keep_their_own_hover() {
    const MENU: &str = "(function(){document.body.innerHTML='<style>#m{display:none}#w:hover #m{display:block}</style><div id=w style=\"position:fixed;left:20px;top:20px;width:200px;height:80px\"><span>Account</span><a id=m href=#out>Sign out</a></div><button id=b style=\"position:fixed;left:20px;top:200px;width:120px;height:40px\">other</button>';return 'ok';})()";
    let shown = "String(getComputedStyle(document.getElementById('m')).display)";
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (s, first, _t) = open_fixture(&ctx, "/form.html");
        eval_js(&ctx, &first, MENU);
        let r = ctx.vs(&["open", &ctx.url("/form.html"), "--session", &s]);
        assert_ok("open second", &r);
        let second = body_first(&r);
        eval_js(&ctx, &second, MENU);
        // A fresh page is not hovered.
        assert_eq!(
            eval_js(&ctx, &second, shown).trim(),
            "none",
            "fresh page hovered"
        );
        let r = ctx.vs(&["hover-at", &first, "60", "40", "-M", "robotic"]);
        assert_ok("hover first", &r);
        assert_eq!(
            eval_js(&ctx, &first, shown).trim(),
            "block",
            "first hovered"
        );
        // The other page's input must not take the first page's hover.
        let r = ctx.vs(&["view", &second, "--full"]);
        let token = token_of(&r);
        let r = ctx.vs(&[
            "click-at",
            &second,
            "60",
            "220",
            "-M",
            "robotic",
            &format!("--token={token}"),
        ]);
        assert_ok("click second", &r);
        settle(200);
        assert_eq!(
            eval_js(&ctx, &first, shown).trim(),
            "block",
            "first page lost its hover to the second page's click"
        );
    }
}

// With several pages open, a click goes to the page it names. On
// Linux every host window sat at the screen origin and XTest input
// goes by position, so the newest window used to take the click.
#[test]
fn cell_click_at_reaches_its_page_under_another() {
    const BUTTON: &str = "(function(){document.body.innerHTML='<button id=b style=\"position:fixed;left:40px;top:40px;width:120px;height:40px\">hit me</button>';window.__hit=0;document.getElementById('b').addEventListener('click',function(e){window.__hit+=(e.isTrusted?1:100);});return 'ok';})()";
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (s, first, _t) = open_fixture(&ctx, "/form.html");
        eval_js(&ctx, &first, BUTTON);
        let r = ctx.vs(&["open", &ctx.url("/form.html"), "--session", &s]);
        assert_ok("open second", &r);
        let second = body_first(&r);
        eval_js(&ctx, &second, BUTTON);
        settle(300);
        for page in [&first, &second, &first] {
            let r = ctx.vs(&["view", page, "--full"]);
            let token = token_of(&r);
            let before = eval_js(&ctx, page, "String(window.__hit)");
            let r = ctx.vs(&[
                "click-at",
                page,
                "100",
                "60",
                "-M",
                "robotic",
                &format!("--token={token}"),
            ]);
            assert_ok("click-at", &r);
            let after = eval_js(&ctx, page, "String(window.__hit)");
            assert_eq!(
                after.parse::<u32>().unwrap(),
                before.parse::<u32>().unwrap() + 1,
                "click lands on {page}"
            );
        }
        assert_eq!(eval_js(&ctx, &first, "String(window.__hit)"), "2");
        assert_eq!(eval_js(&ctx, &second, "String(window.__hit)"), "1");
    }
}

// A viewport smaller than the host window must not shift pointer
// input. WebKitGTK centered the resized web view in its 1280x800
// window while clicks assumed it sat at the top-left, so every
// click-at after `vs viewport` missed by the gap.
#[test]
fn cell_click_at_lands_after_viewport_resize() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/form.html");
        let r = ctx.vs(&["viewport", &page, "1024x600"]);
        assert_ok("viewport", &r);
        eval_js(
            &ctx,
            &page,
            "(function(){document.body.innerHTML='<button id=b style=\"position:fixed;left:40px;top:40px;width:120px;height:40px\">hit me</button>';document.getElementById('b').addEventListener('click',function(e){window.__hit=(window.__hit||0)+(e.isTrusted?1:100);});return 'ok';})()",
        );
        settle(300);
        let r = ctx.vs(&["view", &page, "--full"]);
        let token = token_of(&r);
        let r = ctx.vs(&[
            "click-at",
            &page,
            "100",
            "60",
            "-M",
            "robotic",
            &format!("--token={token}"),
        ]);
        assert_ok("click-at", &r);
        assert_eq!(
            eval_js(&ctx, &page, "String(window.__hit)"),
            "1",
            "one trusted click must land on the button after the resize"
        );
    }
}

// A wheel turn is a native, trusted wheel event: it scrolls what is
// under the pointer (an overflow box, or the page) and the page's
// listener sees isTrusted with the turn's direction.
#[test]
fn cell_wheel_scrolls_what_is_under_the_pointer() {
    let num = |s: &str| s.trim().parse::<f64>().unwrap_or(0.0);
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/wheel.html");
        let poll = |expr: &str| {
            let mut v = String::new();
            for _ in 0..40 {
                settle(50);
                v = eval_js(&ctx, &page, expr);
                if num(&v) > 0.0 {
                    break;
                }
            }
            v
        };

        // Over the overflow box, the box scrolls and the page does not.
        let r = ctx.vs(&["wheel", &page, "500", "120", "0", "200", "--mode=robotic"]);
        assert_ok("wheel box", &r);
        let boxed = poll("String(document.getElementById('box').scrollTop)");
        let all = eval_js(&ctx, &page, "JSON.stringify(window.__wheels)");
        assert!(num(&boxed) > 0.0, "box did not scroll: {boxed} {all}");
        assert!(all.contains("\"trusted\":true"), "wheel event: {all}");
        assert!(
            num(&eval_js(&ctx, &page, "String(window.scrollY)")) < 0.5,
            "the page scrolled with the box"
        );

        // Elsewhere the page scrolls.
        let r = ctx.vs(&["wheel", &page, "100", "300", "0", "300", "--mode=robotic"]);
        assert_ok("wheel page", &r);
        let y = poll("String(window.scrollY)");
        assert!(num(&y) > 0.0, "page did not scroll: scrollY={y}");
        let dy = eval_js(
            &ctx,
            &page,
            "String(window.__wheels.reduce((a, w) => a + w.dy, 0))",
        );
        assert!(num(&dy) > 0.0, "deltaY sum: {dy}");

        // Upwards and sideways come out with their signs.
        let r = ctx.vs(&[
            "wheel",
            &page,
            "100",
            "300",
            "-40",
            "-100",
            "--mode=robotic",
        ]);
        assert_ok("wheel up", &r);
        settle(200);
        let all = eval_js(&ctx, &page, "JSON.stringify(window.__wheels)");
        eprintln!("wheels: {all}");
        let up = eval_js(&ctx, &page, "String(window.__wheels.some(w => w.dy < 0))");
        let left = eval_js(&ctx, &page, "String(window.__wheels.some(w => w.dx < 0))");
        assert_eq!((up.trim(), left.trim()), ("true", "true"), "{all}");
    }
}
