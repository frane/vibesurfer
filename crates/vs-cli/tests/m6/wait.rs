//! Wait cells: stable / net-idle / ref / gone / text / token-change.

use crate::helpers::{open_fixture, ref_for};
use crate::support::{assert_ok, body_rest, each_available_backend, TestContext};

// 15. vs_wait stable
#[test]
fn cell_wait_stable() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/dynamic.html");
        let r = ctx.vs(&["wait", &page, "stable", "--timeout=3000"]);
        assert_ok("wait stable", &r);
    }
}

// 16. vs_wait net-idle
#[test]
fn cell_wait_net_idle() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/network-mixed.html");
        let r = ctx.vs(&["wait", &page, "net-idle", "--timeout=5000"]);
        assert_ok("wait net-idle", &r);
    }
}

// 17. vs_wait ref — initial-item is in the markup at page load; the
// wait-ref predicate succeeds on first poll. The fixture's mutations
// are click-driven, so this test never races with a load timer.
#[test]
fn cell_wait_ref() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/dynamic.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let n = ref_for(&body, "li", "initial item");
        let r = ctx.vs(&["wait", &page, "ref", &n.to_string(), "--timeout=1000"]);
        assert_ok("wait ref", &r);
    }
}

// 18. vs_wait gone — find the initial-item AND the trigger button in
// the load-time snapshot, click the trigger to start the mutation
// timer, then wait for the removal. The click guarantees the
// initial-item was present at view time (no race with a load timer).
#[test]
fn cell_wait_gone() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, t0) = open_fixture(&ctx, "/dynamic.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let item = ref_for(&body, "li", "initial item");
        let trigger = ref_for(&body, "btn", "Run mutations");
        let r = ctx.vs(&[
            "act",
            &page,
            &trigger.to_string(),
            "click",
            &format!("--token={t0}"),
        ]);
        assert_ok("act click trigger", &r);
        let r = ctx.vs(&["wait", &page, "gone", &item.to_string(), "--timeout=3000"]);
        assert_ok("wait gone", &r);
    }
}

// 19. vs_wait text — click the trigger, then wait for "late arrival"
// to appear.
#[test]
fn cell_wait_text() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, t0) = open_fixture(&ctx, "/dynamic.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let trigger = ref_for(&body, "btn", "Run mutations");
        let r = ctx.vs(&[
            "act",
            &page,
            &trigger.to_string(),
            "click",
            &format!("--token={t0}"),
        ]);
        assert_ok("act click trigger", &r);
        let r = ctx.vs(&["wait", &page, "text", "late arrival", "--timeout=3000"]);
        assert_ok("wait text", &r);
    }
}

// 20. vs_wait token-change — click the trigger to drive a DOM
// mutation, then wait for the token to change.
#[test]
fn cell_wait_token_change() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, t0) = open_fixture(&ctx, "/dynamic.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let trigger = ref_for(&body, "btn", "Run mutations");
        let r = ctx.vs(&[
            "act",
            &page,
            &trigger.to_string(),
            "click",
            &format!("--token={t0}"),
        ]);
        assert_ok("act click trigger", &r);
        let r = ctx.vs(&["wait", &page, "token-change", "--timeout=3000"]);
        assert_ok("wait token-change", &r);
    }
}
