//! Shared helpers used by every M6 test group: fixture-loading shim,
//! tree-body ref lookup, JS eval shortcut, settle delay.

use std::time::Duration;

use crate::support::{assert_ok, body_first, token_of, TestContext};

/// Open a session + page on `fixture` (e.g. "/static.html"), call
/// `vs view` to get a baseline token, return `(session, page, token)`.
pub fn open_fixture(ctx: &TestContext, fixture: &str) -> (String, String, String) {
    let r = ctx.vs(&["session-open"]);
    assert_ok("session-open", &r);
    let session = body_first(&r);
    let url = ctx.url(fixture);
    let r = ctx.vs(&["open", &url]);
    assert_ok("open", &r);
    let page = body_first(&r);
    let r = ctx.vs(&["view", &page]);
    assert_ok("view", &r);
    let token = token_of(&r);
    (session, page, token)
}

/// Find a tree ref whose role + label substring match. Returns the
/// refs index parsed from the wire form. Wire format is
/// `<N> <role> "<label>"` with the label optionally bare-quoted (no
/// quotes if it's a single non-whitespace token). Panics if no match.
pub fn ref_for(view_body: &str, role: &str, label_substr: &str) -> u32 {
    for line in view_body.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let mut it = trimmed.splitn(3, ' ');
        let Some(n_str) = it.next() else { continue };
        let Ok(n) = n_str.parse::<u32>() else {
            continue;
        };
        let Some(line_role) = it.next() else { continue };
        let Some(rest) = it.next() else { continue };
        let label = if let Some(after) = rest.strip_prefix('"') {
            match after.find('"') {
                Some(end) => &after[..end],
                None => continue,
            }
        } else {
            rest.split(' ').next().unwrap_or(rest)
        };
        if line_role == role && label.contains(label_substr) {
            return n;
        }
    }
    panic!("no ref with role={role:?} label~{label_substr:?} in view body:\n{view_body}");
}

/// Eval JS via vs_inspect eval, returning the trimmed `result=...`
/// payload from the wire response. Fails the test if eval returned
/// EVAL_ERROR / EVAL_SYNTAX.
pub fn eval_js(ctx: &TestContext, page: &str, expr: &str) -> String {
    let r = ctx.vs(&["inspect", page, "eval", expr, "--full"]);
    assert_ok(&format!("eval {expr}"), &r);
    for line in r.stdout.lines() {
        if let Some(rest) = line.strip_prefix("result=") {
            return rest.to_string();
        }
    }
    panic!("no result= line in eval response:\n{}", r.stdout)
}

/// Sleep — used to let async JS in fixtures settle before assertions.
pub fn settle(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}
