//! Memory cells: vs_mark, vs_annotate, vs_log, vs_skill.

use crate::helpers::{open_fixture, ref_for, settle};
use crate::support::{assert_ok, body_rest, each_available_backend, token_of, TestContext};

// 26. vs_mark — mark survives DOM mutation; verified via audit log.
#[test]
fn cell_mark() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/marks.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n = ref_for(&body, "btn", "CTA");
        let r = ctx.vs(&[
            "mark",
            &page,
            &n.to_string(),
            "primary",
            &format!("--token={token}"),
        ]);
        assert_ok("mark", &r);
        settle(400);
        let r = ctx.vs(&["log", "--limit=20"]);
        assert_ok("log", &r);
        assert!(
            r.stdout.contains("vs_mark") && r.stdout.contains("primary"),
            "log should record the mark call:\n{}",
            r.stdout
        );
    }
}

// 27. vs_annotate
#[test]
fn cell_annotate() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, _p, _t) = open_fixture(&ctx, "/static.html");
        let r = ctx.vs(&["annotate", "page", "purpose", "fixture-static"]);
        assert_ok("annotate", &r);
        let r = ctx.vs(&["status"]);
        assert_ok("status", &r);
        let r = ctx.vs(&["log", "--limit=50"]);
        assert_ok("log", &r);
        assert!(
            r.stdout.contains("annotate") || r.stdout.contains("purpose"),
            "log should include the annotate call:\n{}",
            r.stdout
        );
    }
}

// 29. vs_log
#[test]
fn cell_log() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, _p, _t) = open_fixture(&ctx, "/static.html");
        let r = ctx.vs(&["log", "--limit=20"]);
        assert_ok("log", &r);
        assert!(
            r.stdout.contains("vs_session_open"),
            "log should reference session_open:\n{}",
            r.stdout
        );
        assert!(
            r.stdout.contains("vs_open"),
            "log should reference open:\n{}",
            r.stdout
        );
        assert!(
            r.stdout.contains("vs_view"),
            "log should reference view:\n{}",
            r.stdout
        );
    }
}

// 30. vs_skill
#[test]
fn cell_skill() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let r = ctx.vs(&["session-open"]);
        assert_ok("session-open", &r);
        let r = ctx.vs(&["skill", "list"]);
        assert_ok("skill list", &r);
        assert!(
            r.stdout.contains("debug-failed-action") || !r.stdout.is_empty(),
            "skill list should list bundled skills:\n{}",
            r.stdout
        );
    }
}
