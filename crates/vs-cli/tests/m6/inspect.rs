//! Inspect cells: console / network / request / eval / storage×4 /
//! scripts / script / dom / performance — plus the capability-flag
//! gate that proves `! ENGINE_UNSUPPORTED` flows out when the install
//! path was disabled.

use crate::helpers::{eval_js, open_fixture, ref_for, settle};
use crate::support::{assert_ok, body_rest, each_available_backend, TestContext};

// 36. vs_inspect console — five levels appear post-fixture-load.
#[test]
fn cell_inspect_console() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/console-noisy.html");
        settle(300);
        let r = ctx.vs(&["inspect", &page, "console"]);
        assert_ok("inspect console", &r);
        let body = body_rest(&r);
        for needle in [
            "hello from console.log",
            "info-level message",
            "warn-level message",
            "error-level message",
        ] {
            assert!(
                body.contains(needle),
                "inspect console must include {needle:?}; got:\n{body}"
            );
        }
    }
}

// 37. vs_inspect network — fetches recorded.
#[test]
fn cell_inspect_network() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/network-mixed.html");
        let _ = ctx.vs(&["wait", &page, "net-idle", "--timeout=4000"]);
        let r = ctx.vs(&["inspect", &page, "network"]);
        assert_ok("inspect network", &r);
        let body = body_rest(&r);
        assert!(
            body.contains("/api/ok"),
            "network must include /api/ok; got:\n{body}"
        );
        assert!(
            body.contains("/api/missing"),
            "network must include /api/missing; got:\n{body}"
        );
    }
}

// 38. vs_inspect request — full headers/body for one entry.
#[test]
fn cell_inspect_request() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/network-mixed.html");
        let _ = ctx.vs(&["wait", &page, "net-idle", "--timeout=4000"]);
        let r = ctx.vs(&["inspect", &page, "network"]);
        assert_ok("inspect network", &r);
        let body = body_rest(&r);
        let seq = body
            .lines()
            .find(|l| l.contains("/api/ok"))
            .and_then(|l| l.split_whitespace().next())
            .unwrap_or("");
        assert!(
            seq.starts_with("n_"),
            "expected n_<seq> id on network line; got {seq:?}\n{body}"
        );
        let bare = seq.trim_start_matches("n_");
        let r = ctx.vs(&["inspect", &page, "request", bare]);
        assert_ok("inspect request", &r);
        assert!(
            r.stdout.contains("/api/ok"),
            "inspect request must surface the URL:\n{}",
            r.stdout
        );
    }
}

// 39. vs_inspect eval
#[test]
fn cell_inspect_eval() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/static.html");
        let title = eval_js(&ctx, &page, "document.title");
        assert!(
            title.contains("Static fixture"),
            "eval document.title against real page should match fixture: got {title:?}"
        );
        let two = eval_js(&ctx, &page, "1+1");
        assert!(two.trim() == "2", "1+1 should eval to 2; got {two:?}");
    }
}

// 40. vs_inspect storage cookies
#[test]
fn cell_inspect_storage_cookies() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/storage.html");
        let _ = eval_js(&ctx, &page, "new Promise(r=>setTimeout(()=>r('ok'),200))");
        settle(250);
        let r = ctx.vs(&["inspect", &page, "storage", "cookies"]);
        assert_ok("inspect storage cookies", &r);
        let body = body_rest(&r);
        assert!(
            body.contains("consent"),
            "storage cookies must include consent cookie:\n{body}"
        );
    }
}

// 41. vs_inspect storage local
#[test]
fn cell_inspect_storage_local() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/storage.html");
        settle(250);
        let r = ctx.vs(&["inspect", &page, "storage", "local"]);
        assert_ok("inspect storage local", &r);
        let body = body_rest(&r);
        assert!(
            body.contains("theme"),
            "storage local must include theme key:\n{body}"
        );
    }
}

// 42. vs_inspect storage session
#[test]
fn cell_inspect_storage_session() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/storage.html");
        settle(250);
        let r = ctx.vs(&["inspect", &page, "storage", "session"]);
        assert_ok("inspect storage session", &r);
        let body = body_rest(&r);
        assert!(
            body.contains("csrf"),
            "storage session must include csrf key:\n{body}"
        );
    }
}

// 43. vs_inspect storage indexeddb — two-call to settle the Promise.
#[test]
fn cell_inspect_storage_indexeddb() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/storage.html");
        settle(400);
        let _ = ctx.vs(&["inspect", &page, "storage", "indexeddb"]);
        settle(300);
        let r = ctx.vs(&["inspect", &page, "storage", "indexeddb"]);
        assert_ok("inspect storage indexeddb", &r);
        let body = body_rest(&r);
        assert!(
            body.contains("vibesurfer_demo_db"),
            "storage indexeddb must include the demo db:\n{body}"
        );
    }
}

// 44. vs_inspect scripts
#[test]
fn cell_inspect_scripts() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/scripts.html");
        settle(200);
        let r = ctx.vs(&["inspect", &page, "scripts"]);
        assert_ok("inspect scripts", &r);
        let body = body_rest(&r);
        assert!(
            body.contains("script-a.js"),
            "scripts must list script-a.js:\n{body}"
        );
        assert!(
            body.contains("script-b.js"),
            "scripts must list script-b.js:\n{body}"
        );
    }
}

// 45. vs_inspect script — body of a specific entry.
#[test]
fn cell_inspect_script() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/scripts.html");
        settle(200);
        let r = ctx.vs(&["inspect", &page, "scripts"]);
        assert_ok("inspect scripts", &r);
        let body = body_rest(&r);
        let line = body
            .lines()
            .find(|l| l.contains("script-a.js"))
            .unwrap_or("");
        let seq = line.split_whitespace().next().unwrap_or("");
        let bare = seq.trim_start_matches("s_").trim_start_matches("n_");
        let r = ctx.vs(&["inspect", &page, "script", bare]);
        assert_ok("inspect script", &r);
        assert!(
            r.stdout.contains("__scriptA") || r.stdout.contains("script-a"),
            "inspect script must surface script source:\n{}",
            r.stdout
        );
    }
}

// 46. vs_inspect dom
#[test]
fn cell_inspect_dom() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/static.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let n = ref_for(&body, "btn", "Submit");
        let r = ctx.vs(&["inspect", &page, "dom", &n.to_string()]);
        assert_ok("inspect dom", &r);
        assert!(
            r.stdout.contains("button") || r.stdout.contains("Submit"),
            "inspect dom must surface real DOM for the ref:\n{}",
            r.stdout
        );
    }
}

// 47. vs_inspect performance
#[test]
fn cell_inspect_performance() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/perf.html");
        settle(500);
        let r = ctx.vs(&["inspect", &page, "performance"]);
        assert_ok("inspect performance", &r);
        let body = body_rest(&r);
        let lower = body.to_lowercase();
        assert!(
            lower.contains("ttfb") || lower.contains("fcp") || lower.contains("lcp"),
            "performance must include at least one Web Vital key:\n{body}"
        );
    }
}

// 48. Capability-flag honesty: when install path fails, the wire
// returns `! ENGINE_UNSUPPORTED <op>`, not an empty buffer.
#[test]
fn cell_engine_unsupported_when_install_disabled() {
    for backend in each_available_backend() {
        eprintln!("== cell_engine_unsupported: backend={} ==", backend.name());
        let ctx = TestContext::start_with_env(&[("VS_DISABLE_INSPECTOR", "1")]);
        let (_s, page, _t) = open_fixture(&ctx, "/console-noisy.html");
        let r = ctx.vs(&["inspect", &page, "console"]);
        assert_ne!(
            r.code, 0,
            "expected non-zero exit when inspector install was disabled; stdout={:?}",
            r.stdout
        );
        assert!(
            r.stdout.contains("ENGINE_UNSUPPORTED"),
            "expected ENGINE_UNSUPPORTED on disabled inspector; got:\n{}",
            r.stdout
        );
        let r = ctx.vs(&["inspect", &page, "network"]);
        assert!(
            r.stdout.contains("ENGINE_UNSUPPORTED"),
            "expected ENGINE_UNSUPPORTED on disabled inspector network; got:\n{}",
            r.stdout
        );
    }
}
