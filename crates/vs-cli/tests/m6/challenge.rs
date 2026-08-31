//! Bot-challenge visibility cells.
//!
//! A page gated by Turnstile / hCaptcha / reCAPTCHA used to be
//! indistinguishable from an ordinary page: the challenge container is
//! a bare div the role table ignores, a widget that fails to render
//! leaves no iframe, and the surviving hidden input read as an
//! anonymous `tf ... hid=1`. The agent filled the form, submitted, and
//! the server refused it with nothing in the tree to explain why.
//!
//! These cells pin the two things that fix that: the node is labelled
//! and carries `challenge=<provider>:<state>`, and `vs view` raises
//! `? captcha_visible`.

use crate::helpers::open_fixture;
use crate::support::{assert_ok, body_rest, each_available_backend, TestContext};

/// An unsolved challenge must be visible in the tree *and* announced
/// on the envelope, with a box an agent can aim a click at.
#[test]
fn cell_challenge_pending_is_announced() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/challenge.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        assert_ok("view", &r);

        assert!(
            r.stdout.contains("? captcha_visible turnstile pending"),
            "an unsolved challenge must raise ? captcha_visible:\n{}",
            r.stdout
        );

        let body = body_rest(&r);
        assert!(
            body.contains("challenge=turnstile:pending"),
            "the challenge node must carry its provider and state:\n{body}"
        );
        assert!(
            body.contains("challenge_box="),
            "the node must carry a box so an agent can click it:\n{body}"
        );
        assert!(
            body.contains("turnstile challenge"),
            "the challenge node must be labelled, not anonymous:\n{body}"
        );
    }
}

/// Exactly one node per challenge. Implicit rendering matches both the
/// container and the response input inside it; only the outermost
/// should be tagged, or a single gate reports as two.
#[test]
fn cell_challenge_is_reported_once() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/challenge.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        assert_ok("view", &r);
        let body = body_rest(&r);

        let tagged = body.matches("challenge=turnstile:").count();
        assert_eq!(
            tagged, 1,
            "expected exactly one tagged turnstile node, got {tagged}:\n{body}"
        );
    }
}

/// A solved challenge is tagged but must NOT warn — the page is not
/// blocked, and a warning on every subsequent view would be noise the
/// agent learns to ignore.
#[test]
fn cell_solved_challenge_does_not_warn() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/challenge.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        assert_ok("view", &r);
        let body = body_rest(&r);

        assert!(
            body.contains("challenge=hcaptcha:solved"),
            "a filled response field means solved:\n{body}"
        );
        assert!(
            !r.stdout.contains("captcha_visible hcaptcha"),
            "a solved challenge must not warn:\n{}",
            r.stdout
        );
    }
}
