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
