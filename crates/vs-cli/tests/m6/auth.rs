//! Auth cells: vs_auth save / load.

use crate::helpers::{eval_js, open_fixture, ref_for};
use crate::support::{
    assert_ok, body_first, body_rest, each_available_backend, token_of, TestContext,
};

// 34. vs_auth save — round-trip cookie.
#[test]
fn cell_auth_save() {
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
        assert_ok("submit login", &r);
        let _ = ctx.vs(&["wait", &page, "stable", "--timeout=2000"]);
        let r = ctx.vs(&["auth", "save", &page, "fixture-auth"]);
        assert_ok("auth save", &r);
        let r = ctx.vs(&["auth", "list"]);
        assert_ok("auth list", &r);
        assert!(
            r.stdout.contains("fixture-auth"),
            "auth list should include saved blob:\n{}",
            r.stdout
        );
    }
}

// 35. vs_auth load — apply blob, dashboard renders.
#[test]
fn cell_auth_load() {
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
        let _ = ctx.vs(&[
            "act",
            &page,
            &n_email.to_string(),
            "submit",
            &format!("--token={token}"),
        ]);
        let _ = ctx.vs(&["wait", &page, "stable", "--timeout=2000"]);
        let r = ctx.vs(&["auth", "save", &page, "fixture-auth"]);
        assert_ok("auth save", &r);
        let r = ctx.vs(&["open", &ctx.url("/dashboard")]);
        assert_ok("open dashboard fresh", &r);
        let new_page = body_first(&r);
        let r = ctx.vs(&["auth", "load", &new_page, "fixture-auth"]);
        assert_ok("auth load", &r);
        let r = ctx.vs(&["open", &ctx.url("/dashboard")]);
        assert_ok("open dashboard after load", &r);
        let p2 = body_first(&r);
        let title = eval_js(&ctx, &p2, "document.title");
        assert!(
            title.contains("Dashboard"),
            "after auth load the dashboard page must render; got {title:?}"
        );
    }
}
// 36. vs_auth save with an HttpOnly session cookie present.
//
// Regression for the v0.1.1 bug: `auth save` used `document.cookie`
// to scrape the cookie jar, which by spec cannot see `HttpOnly`
// cookies. After login on any modern web app the auth cookie was
// silently dropped; the saved blob held only localStorage and any
// non-HttpOnly cookies, and `auth load` had nothing to restore.
//
// v0.1.2 routes the cookie portion through the engine's host-side
// cookie store API (`WKHTTPCookieStore` on macOS), which sees
// `HttpOnly` because the attribute hides the cookie from JS only.
// This cell verifies the save path no longer crashes or no-ops on
// an HttpOnly cookie and that the blob round-trips through load on
// a fresh page. The two-page round-trip is the load-side smoke;
// the deeper proof (HttpOnly preserved bit-for-bit through the
// JSON layer) is the auth.rs unit tests.
#[test]
fn cell_auth_http_only_save_and_load() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let r = ctx.vs(&["session-open"]);
        assert_ok("session-open", &r);

        // Hit the HttpOnly login endpoint. Server returns
        // `Set-Cookie: ht_session=...; HttpOnly`, which lands in the
        // engine's cookie store via the network response parser, not
        // via JS — so `document.cookie` does NOT see it.
        let r = ctx.vs(&["open", &ctx.url("/login-httponly")]);
        assert_ok("open /login-httponly", &r);
        let page = body_first(&r);
        let _ = ctx.vs(&["wait", &page, "stable", "--timeout=3000"]);

        // Save: the host-side path captures ht_session. On v0.1.1
        // this returned success but produced an empty cookies field
        // in the blob.
        let r = ctx.vs(&["auth", "save", &page, "ht-fixture"]);
        assert_ok("auth save with HttpOnly cookie present", &r);
        let r = ctx.vs(&["auth", "list"]);
        assert!(
            r.stdout.contains("ht-fixture"),
            "auth list missing 'ht-fixture' after save:\n{}",
            r.stdout
        );

        // Load on a fresh page in the same session. The cookie is
        // already in the engine's process-wide store (so this
        // doesn't prove load alone restored it — that's covered by
        // the auth.rs JSON round-trip unit tests), but it does
        // confirm the load call accepts the new structured blob
        // shape and applies cookies without error.
        let r = ctx.vs(&["open", &ctx.url("/dashboard-httponly")]);
        let dash = body_first(&r);
        let r = ctx.vs(&["auth", "load", &dash, "ht-fixture"]);
        assert_ok("auth load", &r);
        let r = ctx.vs(&["open", &ctx.url("/dashboard-httponly")]);
        let after = body_first(&r);
        let r = ctx.vs(&["view", &after, "--full"]);
        let body = body_rest(&r);
        assert!(
            !body.contains("unauthenticated"),
            "dashboard 401 after auth load + nav; cookie not in store:\n{body}"
        );
    }
}
