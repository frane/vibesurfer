//! Popups, dialogs and crash events, polled through `vs net`.

use std::time::{Duration, Instant};

use crate::support::{assert_err, assert_ok, body_first, each_available_backend, vs, TestContext};

use super::harness::{eval, eventually, poll, purl, sv, Ev};

// `--popups`: a window.open() becomes its own page in the session, with
// `window.opener`, and the opener stays where it was. The `page` event
// reaches the poll stream before the popup's first request, so a client
// can tell whose request it is; `window.close()` reports `close`. Other
// engines still load new windows in place.
#[test]
fn cell_popups_open_their_own_page() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let r = ctx.vs(&["session-open", "--intercept", "--popups"]);
        assert_ok("session-open --popups", &r);
        let s = body_first(&r);
        let r = sv(&ctx, &s, &["open", &purl(&ctx, "/popup.html")]);
        assert_ok("open", &r);
        let page = body_first(&r);
        poll(&ctx, &s, 0);

        let r = sv(&ctx, &s, &["view", &page, "--full"]);
        let target = crate::helpers::ref_for(&crate::support::body_rest(&r), "btn", "Window open");
        let token = r
            .stdout
            .lines()
            .next()
            .unwrap_or("")
            .trim_start_matches('@')
            .to_string();
        let r = sv(
            &ctx,
            &s,
            &[
                "act",
                &page,
                &target.to_string(),
                "click",
                &format!("--token={token}"),
            ],
        );
        assert_ok("click", &r);

        let start = Instant::now();
        let mut evs = Vec::new();
        while start.elapsed() < Duration::from_secs(10) && !evs.iter().any(|e: &Ev| e.kind == "fin")
        {
            evs.extend(poll(&ctx, &s, 500));
        }
        let at = |kind: &str| evs.iter().position(|e| e.kind == kind);
        // The popup's own request: WebView2 also sends Edge's background
        // traffic through the proxy.
        let pg = at("page").expect("page event");
        let req = evs
            .iter()
            .position(|e| e.kind == "req" && e.url().ends_with("/multi-page-2.html"))
            .expect("popup request");
        assert!(pg < req, "page before its request: {evs:#?}");
        // page <opener> <popup> <url>
        let line = &evs[pg].line;
        assert!(line.ends_with("/multi-page-2.html"), "{line}");
        let popup = line.split(' ').next().unwrap_or("").to_string();

        assert_eq!(
            eventually(&ctx, &s, &popup, "document.title", "Multi-page B"),
            "Multi-page B"
        );
        assert_eq!(eval(&ctx, &s, &popup, "String(!!window.opener)"), "true");
        assert!(eval(&ctx, &s, &page, "location.pathname").ends_with("/popup.html"));
        let r = sv(&ctx, &s, &["status"]);
        assert!(
            r.stdout.contains(&popup),
            "status lists the popup: {}",
            r.stdout
        );

        // The page may close before the eval's reply is read, and the
        // daemon then answers NOT_FOUND for it; either is a close.
        let r = sv(
            &ctx,
            &s,
            &["inspect", &popup, "eval", "(window.close(), 1)", "--full"],
        );
        assert!(
            r.stdout.contains("result=1") || r.stdout.starts_with("! NOT_FOUND"),
            "window.close(): {}",
            r.stdout
        );
        let start = Instant::now();
        let mut closed = false;
        while start.elapsed() < Duration::from_secs(10) && !closed {
            closed = poll(&ctx, &s, 500).iter().any(|e| e.kind == "close");
        }
        assert!(closed, "close event");
        let r = sv(&ctx, &s, &["status"]);
        assert!(
            !r.stdout.contains(&popup),
            "closed popup gone: {}",
            r.stdout
        );
    }
}

// `--popups` sessions report `alert`/`confirm`/`prompt` as `dialog`
// lines and hold them for `vs net dialog`, even while the click that
// opened one is still running; the click returns after the answer.
#[test]
fn cell_dialogs_wait_for_an_answer() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let r = ctx.vs(&["session-open", "--intercept", "--popups"]);
        assert_ok("session-open --popups", &r);
        let s = body_first(&r);
        let r = sv(&ctx, &s, &["open", &purl(&ctx, "/dialogs.html")]);
        assert_ok("open", &r);
        let page = body_first(&r);
        poll(&ctx, &s, 0);

        for (label, answer, want) in [
            ("Confirm", vec!["accept"], "yes"),
            ("Confirm", vec!["dismiss"], "no"),
            ("Prompt", vec!["accept", "--text", "Ada"], "Ada"),
            ("Prompt", vec!["accept"], "def"),
            ("Prompt", vec!["dismiss"], "null"),
        ] {
            let r = sv(&ctx, &s, &["view", &page, "--full"]);
            let target = crate::helpers::ref_for(&crate::support::body_rest(&r), "btn", label);
            let token = r
                .stdout
                .lines()
                .next()
                .unwrap_or("")
                .trim_start_matches('@')
                .to_string();
            let home = ctx.home_path().to_path_buf();
            let (pg, sess) = (page.clone(), s.clone());
            let click = std::thread::spawn(move || {
                vs(
                    &home,
                    &[
                        "act",
                        &pg,
                        &target.to_string(),
                        "click",
                        &format!("--token={token}"),
                        &format!("--session={sess}"),
                    ],
                )
            });
            let start = Instant::now();
            let mut dialog = None;
            while dialog.is_none() && start.elapsed() < Duration::from_secs(10) {
                dialog = poll(&ctx, &s, 500).into_iter().find(|e| e.kind == "dialog");
            }
            // dialog <page> <id> <kind> <message-b64> <default-b64>
            let d = dialog.expect("dialog event");
            let id = d.line.split(' ').next().unwrap_or("").to_string();
            let mut a = vec!["net", "dialog", &id];
            a.extend(answer.iter().copied());
            let seen = start.elapsed();
            assert_ok("net dialog", &sv(&ctx, &s, &a));
            assert_ok("click", &click.join().unwrap());
            // A closed dialog refuses a second answer instead of
            // reporting success the page never saw.
            assert_eq!(assert_err(&sv(&ctx, &s, &a)), "NOT_FOUND", "answer twice");
            let got = eval(&ctx, &s, &page, "document.getElementById('o').textContent");
            if got != want {
                // Say what else happened, for a failure seen only on CI.
                let more: Vec<String> = poll(&ctx, &s, 0).into_iter().map(|e| e.line).collect();
                let log =
                    std::fs::read_to_string(ctx.home_path().join("daemon.log")).unwrap_or_default();
                let tail: Vec<&str> = log.lines().rev().take(40).collect();
                panic!(
                    "{label} {answer:?}: got {got:?}, want {want:?}; dialog {} seen after {seen:?}, \
                     whole case {:?}; later events {more:?}\n--- daemon.log tail ---\n{}",
                    d.line,
                    start.elapsed(),
                    tail.into_iter().rev().collect::<Vec<_>>().join("\n")
                );
            }
        }
    }
}

// (u) A page whose web content process dies is reported as `crash`.
#[test]
fn cell_crash_is_reported() {
    for _ in each_available_backend() {
        let ctx = TestContext::start_with_env(&[("VS_ALLOW_CRASH", "1")]);
        let r = ctx.vs(&["session-open", "--intercept", "--popups"]);
        assert_ok("session-open", &r);
        let s = body_first(&r);
        let r = sv(&ctx, &s, &["open", &purl(&ctx, "/net.html")]);
        assert_ok("open", &r);
        let page = body_first(&r);
        let r = sv(&ctx, &s, &["debug-crash", &page]);
        assert_ok("debug-crash", &r);
        let start = Instant::now();
        let mut seen = false;
        while !seen && start.elapsed() < Duration::from_secs(10) {
            let r = sv(&ctx, &s, &["net", "poll", "--wait-ms", "300"]);
            seen = r.stdout.lines().any(|l| l == format!("crash {page}"));
        }
        assert!(seen, "no crash line for {page}");
    }
}
