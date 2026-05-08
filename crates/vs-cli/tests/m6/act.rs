//! Act cells: vs_act click / fill / scroll / key / submit / hover / focus.

use crate::helpers::{eval_js, open_fixture, ref_for};
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
        let _ = ctx.vs(&["wait", &page, "stable", "--timeout=2000"]);
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
