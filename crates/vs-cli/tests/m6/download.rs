//! Download cells: getting a file out of a headless page.
//!
//! A headless WKWebView / WebKitGTK / WebView2 has no download
//! delegate, so anything the page tries to save is dropped without an
//! event. These cells cover the three ways out of that: fetch a URL
//! with the page's own credentials, drain a save the page performed
//! itself, and see an embedded frame's `src` in the tree at all.

use crate::helpers::{eval_js, open_fixture, ref_for, settle};
use crate::support::fixture_server::{REPORT_PDF_BODY, REPORT_PDF_NAME};
use crate::support::{
    assert_err, assert_ok, body_rest, each_available_backend, token_of, TestContext,
};

/// Body line lookup for `vs download`'s `key\tvalue` rows.
fn field(out: &str, key: &str) -> String {
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{key}\t")) {
            return rest.to_string();
        }
    }
    panic!("no `{key}` row in download response:\n{out}");
}

/// Give the page the fixture session cookie, so `/files/report.pdf`
/// answers with the file instead of 401.
fn authenticate(ctx: &TestContext, page: &str) {
    eval_js(
        ctx,
        page,
        "document.cookie = 'session_id=ssid-fixture; Path=/'; 'set'",
    );
}

// vs_download <page> <url> — reads with the page's cookie jar.
#[test]
fn cell_download_url_uses_page_session() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/download.html");
        authenticate(&ctx, &page);

        let r = ctx.vs(&["download", &page, "/files/report.pdf"]);
        assert_ok("download url", &r);
        let body = body_rest(&r);

        let path = std::path::PathBuf::from(field(&body, "path"));
        assert!(
            path.exists(),
            "download did not write a file: {}",
            path.display()
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            REPORT_PDF_BODY,
            "downloaded bytes differ from what the fixture served"
        );
        // The name comes from Content-Disposition, not the URL.
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            REPORT_PDF_NAME,
            "Content-Disposition filename was ignored"
        );
        assert_eq!(field(&body, "size"), REPORT_PDF_BODY.len().to_string());
        assert!(field(&body, "mime").contains("pdf"), "mime: {body}");
        // Files land under the daemon's home, not somewhere arbitrary.
        assert!(
            path.starts_with(ctx.home_path()),
            "escaped home: {}",
            path.display()
        );
    }
}

// A download that fails must say so — the whole bug being fixed is
// downloads that vanish with no diagnostic.
#[test]
fn cell_download_failure_is_reported() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/download.html");
        // No cookie set: the fixture answers 401.
        let r = ctx.vs(&["download", &page, "/files/report.pdf"]);
        let code = assert_err(&r);
        assert!(
            r.stdout.contains("401") || code == "TIMEOUT",
            "a 401 must surface as an error, not silence:\n{}",
            r.stdout
        );
    }
}

// The reported bug, end to end: the page saves a blob and revokes the
// object URL immediately. Nothing reaches disk on its own; `vs
// download` with no URL has to produce the bytes anyway.
#[test]
fn cell_download_captures_in_page_blob_save() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/download.html");

        let r = ctx.vs(&["view", &page, "--full"]);
        assert_ok("view", &r);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n_save = ref_for(&body, "btn", "Save blob");

        let r = ctx.vs(&[
            "act",
            &page,
            &n_save.to_string(),
            "click",
            &format!("--token={token}"),
        ]);
        assert_ok("click save", &r);
        settle(500);

        // It shows up as a pending intent...
        let r = ctx.vs(&["download", &page, "--list"]);
        assert_ok("download list", &r);
        assert!(
            r.stdout.contains("saved-from-page.txt"),
            "the save was not captured:\n{}",
            r.stdout
        );

        // ...and drains to a real file.
        let r = ctx.vs(&["download", &page]);
        assert_ok("download captured", &r);
        let body = body_rest(&r);
        let path = std::path::PathBuf::from(field(&body, "path"));
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "saved-from-page.txt",
            "the anchor's download attribute should name the file"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"in-page blob payload",
            "blob payload did not survive the revoked object URL"
        );
    }
}

// An iframe used to be a hole in the tree: no role, no children, so
// `visit` dropped it and its src with it. Without the src there is
// nothing to hand `vs download`.
#[test]
fn cell_iframe_src_is_visible_in_tree() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/download.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        assert_ok("view", &r);
        let body = body_rest(&r);
        assert!(
            body.lines().any(|l| {
                let t = l.trim_start();
                t.split(' ').nth(1) == Some("ifr") && t.contains("/files/report.pdf")
            }),
            "no ifr node carrying the frame src:\n{body}"
        );
    }
}
