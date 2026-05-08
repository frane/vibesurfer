//! Visual cells: vs_capture, vs_viewport, vs_layout.

use crate::helpers::{eval_js, open_fixture, ref_for, settle};
use crate::support::{assert_ok, body_rest, each_available_backend, TestContext};

// 31. vs_capture — PNG to disk.
#[test]
fn cell_capture() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/static.html");
        let r = ctx.vs(&["capture", &page]);
        assert_ok("capture", &r);
        let body = body_rest(&r);
        let path = body.lines().next().unwrap_or("").trim();
        let p = std::path::Path::new(path);
        assert!(p.exists(), "capture path should exist on disk: {path}");
        let bytes = std::fs::read(p).expect("read capture");
        assert!(
            bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
            "capture file must be a PNG; first bytes={:?}",
            &bytes.iter().take(8).collect::<Vec<_>>()
        );
        assert!(
            bytes.len() > 100,
            "capture PNG should be more than a header; len={}",
            bytes.len()
        );
    }
}

// 32. vs_viewport — switch to mobile, observe @media reflow.
#[test]
fn cell_viewport() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/responsive.html");
        let r = ctx.vs(&["viewport", &page, "320x568"]);
        assert_ok("viewport mobile", &r);
        settle(150);
        let mobile = eval_js(
            &ctx,
            &page,
            "getComputedStyle(document.getElementById('nav-mobile')).display",
        );
        assert!(
            mobile.contains("block"),
            "after 320px viewport, hamburger nav should be display:block; got {mobile:?}"
        );
        let desktop = eval_js(
            &ctx,
            &page,
            "getComputedStyle(document.getElementById('nav-desktop')).display",
        );
        assert!(
            desktop.contains("none"),
            "after 320px viewport, desktop nav should be display:none; got {desktop:?}"
        );
    }
}

// 33. vs_layout — non-zero box for visible button.
#[test]
fn cell_layout() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/static.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let n = ref_for(&body, "btn", "Submit");
        let r = ctx.vs(&["layout", &page, &n.to_string()]);
        assert_ok("layout", &r);
        let body = body_rest(&r);
        let line = body.lines().next().unwrap_or("");
        let pick = |key: &str| -> f64 {
            for tok in line.split_whitespace() {
                if let Some(rest) = tok.strip_prefix(key) {
                    return rest.parse().unwrap_or(0.0);
                }
            }
            0.0
        };
        let w = pick("w=");
        let h = pick("h=");
        assert!(
            w > 0.0 && h > 0.0,
            "button should have non-zero size; w={w} h={h} line={line}"
        );
    }
}
