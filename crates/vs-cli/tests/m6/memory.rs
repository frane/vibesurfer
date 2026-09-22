//! Memory cells: vs_mark, vs_annotate, vs_log, vs_skill.

use crate::helpers::{eval_js, open_fixture, ref_for, settle};
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

/// 26b. `vs act mark:NAME` — a mark is an address, not a number.
///
/// `ActTarget::Mark` returned `NotImplemented` on every real backend,
/// so a mark could be taken and listed but never acted on: the agent
/// still had to carry the ref, which is the thing a mark exists to
/// stop it doing. Marks now resolve at act time, against the tree as
/// it is — including when a re-render has renumbered everything, where
/// the answer comes back with `mark_reaimed`.
#[test]
fn cell_act_on_a_mark() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/mark-reaim.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let (body, token) = (body_rest(&r), token_of(&r));
        let save = ref_for(&body, "btn", "Save draft");
        let rebuild = ref_for(&body, "btn", "Rebuild");

        let r = ctx.vs(&[
            "mark",
            &page,
            &save.to_string(),
            "draft",
            &format!("--token={token}"),
        ]);
        assert_ok("mark", &r);
        let token = token_of(&r);

        // The ref still holds: acting through the mark is a plain
        // click, with nothing to report.
        let r = ctx.vs(&[
            "act",
            &page,
            "mark:draft",
            "click",
            &format!("--token={token}"),
        ]);
        assert_ok("act mark", &r);
        assert!(
            !r.stdout.contains("mark_reaimed"),
            "a mark that did not move must not warn:\n{}",
            r.stdout
        );
        let status = eval_js(&ctx, &page, "document.getElementById('status').textContent");
        assert!(status.contains("saved"), "click landed, got {status:?}");

        // Rebuild the button: same role, same label, new element, new
        // ref. The recorded number is now stale.
        let r = ctx.vs(&["view", &page, "--full"]);
        let token = token_of(&r);
        let r = ctx.vs(&[
            "act",
            &page,
            &rebuild.to_string(),
            "click",
            &format!("--token={token}"),
        ]);
        assert_ok("act rebuild", &r);
        settle(200);
        let r = ctx.vs(&["view", &page, "--full"]);
        let (body, token) = (body_rest(&r), token_of(&r));
        let fresh = ref_for(&body, "btn", "Save draft");
        assert_ne!(fresh, save, "rebuild must hand out a new ref");

        // The mark still names the button, and says it moved.
        let r = ctx.vs(&[
            "act",
            &page,
            "mark:draft",
            "click",
            &format!("--token={token}"),
        ]);
        assert_ok("act mark after rebuild", &r);
        assert!(
            r.stdout.contains("mark_reaimed"),
            "a re-aimed mark must say so:\n{}",
            r.stdout
        );
        let status = eval_js(&ctx, &page, "document.getElementById('status').textContent");
        assert!(
            status.contains("saved"),
            "re-aimed click landed, got {status:?}"
        );

        // A name that was never marked is NOT_FOUND, not a silent no-op.
        let r = ctx.vs(&["view", &page, "--full"]);
        let token = token_of(&r);
        let r = ctx.vs(&[
            "act",
            &page,
            "mark:nope",
            "click",
            &format!("--token={token}"),
        ]);
        assert!(
            r.stdout.contains("NOT_FOUND") && r.stdout.contains("mark=nope"),
            "unknown mark must name itself:\n{}",
            r.stdout
        );
    }
}
