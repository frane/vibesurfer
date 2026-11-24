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
            bytes.iter().take(8).collect::<Vec<_>>()
        );
        assert!(
            bytes.len() > 100,
            "capture PNG should be more than a header; len={}",
            bytes.len()
        );
    }
}

// 31b. vs_capture — back-to-back captures after eval/viewport must not
// wedge on the snapshot's wait-for-screen-update. Reproduces the
// sequential-automation TIMEOUT bug (offscreen window never paints
// on-screen, so `afterScreenUpdates=YES` never fires its handler).
#[test]
fn cell_capture_sequential() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/static.html");
        for i in 0..5 {
            let r = ctx.vs(&["viewport", &page, "1200x800"]);
            assert_ok(&format!("viewport #{i}"), &r);
            let ready = eval_js(&ctx, &page, "document.readyState");
            assert!(
                ready.contains("complete"),
                "readyState should be complete on iter {i}; got {ready:?}"
            );
            let r = ctx.vs(&["capture", &page]);
            assert_ok(&format!("capture #{i}"), &r);
            let path = body_rest(&r)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            let bytes = std::fs::read(&path).expect("read capture");
            assert!(
                bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) && bytes.len() > 100,
                "capture #{i} must be a non-trivial PNG; len={}",
                bytes.len()
            );
        }
    }
}

// 31c. vs_capture clean — prune the captures directory. Seeds the
// captures dir directly (rather than via real captures) so the count is
// deterministic across backends — macOS names shots `wk-<page>-<ts>.png`
// (unique) while the Linux/Windows backends use `capture-<page>.png`
// (same-page shots overwrite), which is irrelevant to what `clean` does.
#[test]
fn cell_capture_clean() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let cap_dir = ctx.home_path().join("captures");
        std::fs::create_dir_all(&cap_dir).expect("mk captures dir");
        for i in 0..3 {
            std::fs::write(cap_dir.join(format!("wk-{i}-{i}.png")), b"\x89PNG").unwrap();
        }
        // A non-PNG must survive the prune.
        std::fs::write(cap_dir.join("notes.txt"), b"keep").unwrap();

        let png_count = |dir: &std::path::Path| -> usize {
            std::fs::read_dir(dir).map_or(0, |rd| {
                rd.filter_map(Result::ok)
                    .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
                    .count()
            })
        };
        assert_eq!(png_count(&cap_dir), 3, "seeded 3 PNGs in {cap_dir:?}");

        let r = ctx.vs(&["capture", "clean", "--all"]);
        assert_ok("capture clean --all", &r);
        let body = body_rest(&r);
        assert!(
            body.contains("deleted=3"),
            "clean should report deleted=3; got {body:?}"
        );
        assert_eq!(
            png_count(&cap_dir),
            0,
            "no PNGs should remain after `capture clean --all`"
        );
        assert!(
            cap_dir.join("notes.txt").exists(),
            "clean must not touch non-PNG files"
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

// 31c. vs record --format=webm and the default MP4: a recording of a
// page holds at least one frame and has the container's signature.
#[test]
fn cell_record_formats() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/static.html");
        for (fmt, magic, at) in [
            ("webm", &[0x1A, 0x45, 0xDF, 0xA3][..], 0),
            ("mp4", &b"ftyp"[..], 4),
        ] {
            let r = ctx.vs(&["record", "start", &page, "--fps", "10", "--format", fmt]);
            assert_ok("record start", &r);
            for i in 0..6 {
                let _ = ctx.vs(&[
                    "move-to",
                    &page,
                    &format!("{}", 40 + i * 30),
                    "60",
                    "-M",
                    "careful",
                ]);
                settle(120);
            }
            let r = ctx.vs(&["record", "stop", &page]);
            assert_ok("record stop", &r);
            let body = body_rest(&r);
            let path = body
                .lines()
                .find_map(|l| l.strip_prefix("path\t"))
                .unwrap_or(body.lines().next().unwrap_or(""))
                .trim()
                .to_string();
            assert!(path.ends_with(&format!(".{fmt}")), "{fmt}: path {path}");
            let bytes = std::fs::read(&path).expect("read recording");
            assert_eq!(&bytes[at..at + magic.len()], magic, "{fmt}: bad signature");
            assert!(bytes.len() > 500, "{fmt}: only {} bytes", bytes.len());
        }
    }
}
