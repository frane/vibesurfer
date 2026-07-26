//! Act cells: vs_act click / fill / scroll / key / submit / hover / focus.

use crate::helpers::{eval_js, open_fixture, ref_for, settle};
use crate::support::{assert_ok, body_rest, each_available_backend, token_of, TestContext};

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
        let _ = ctx.vs(&["wait", &page, "stable", "--timeout=2000"]);
        let title = eval_js(&ctx, &page, "document.title");
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
// x.com composer flow via #vibesurfer (01KXN5). macOS-only for now.
#[cfg(target_os = "macos")]
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
