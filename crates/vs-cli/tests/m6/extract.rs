//! Extract cells (table / form / list / jsonld / webmcp) + vs_find.

use crate::helpers::open_fixture;
use crate::support::{assert_ok, body_first, each_available_backend, token_of, TestContext};

// 14. vs_find — cross-page
#[test]
fn cell_find() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, _p1, _t) = open_fixture(&ctx, "/multi-page.html");
        let r = ctx.vs(&["open", &ctx.url("/multi-page-2.html")]);
        assert_ok("open page B", &r);
        let _ = body_first(&r);
        let r = ctx.vs(&["find", "polywomble"]);
        assert_ok("find", &r);
        assert!(
            r.stdout.to_lowercase().contains("polywomble") || r.stdout.contains("p_"),
            "find for polywomble should return hits:\n{}",
            r.stdout
        );
    }
}

// 21. vs_extract table
#[test]
fn cell_extract_table() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/extract.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let token = token_of(&r);
        let r = ctx.vs(&["extract", &page, "table", &format!("--token={token}")]);
        assert_ok("extract table", &r);
        assert!(
            r.stdout.contains("Ada") && r.stdout.contains("Grace"),
            "extract table must return both rows from fixture:\n{}",
            r.stdout
        );
    }
}

// 22. vs_extract form
#[test]
fn cell_extract_form() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/extract.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let token = token_of(&r);
        let r = ctx.vs(&["extract", &page, "form", &format!("--token={token}")]);
        assert_ok("extract form", &r);
        assert!(
            r.stdout.contains("email") && r.stdout.contains("message"),
            "extract form must return email + message fields:\n{}",
            r.stdout
        );
    }
}

// 23. vs_extract list
#[test]
fn cell_extract_list() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/extract.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let token = token_of(&r);
        let r = ctx.vs(&["extract", &page, "list", &format!("--token={token}")]);
        assert_ok("extract list", &r);
        assert!(
            r.stdout.contains("Buy bread")
                && r.stdout.contains("Walk dog")
                && r.stdout.contains("Ship code"),
            "extract list must return all three items:\n{}",
            r.stdout
        );
    }
}

// 24. vs_extract jsonld
#[test]
fn cell_extract_jsonld() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/extract.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let token = token_of(&r);
        let r = ctx.vs(&["extract", &page, "jsonld", &format!("--token={token}")]);
        assert_ok("extract jsonld", &r);
        assert!(
            r.stdout.contains("Demo Widget") && r.stdout.contains("19.99"),
            "extract jsonld must surface the product fields:\n{}",
            r.stdout
        );
    }
}

// 25. vs_extract webmcp
#[test]
fn cell_extract_webmcp() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/extract.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let token = token_of(&r);
        let r = ctx.vs(&["extract", &page, "webmcp", &format!("--token={token}")]);
        assert_ok("extract webmcp", &r);
        assert!(
            r.stdout.contains("lookup") && r.stdout.contains("create"),
            "extract webmcp must surface the tool entries:\n{}",
            r.stdout
        );
    }
}
