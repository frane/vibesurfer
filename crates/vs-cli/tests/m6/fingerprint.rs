//! Fingerprint-integrity cells.
//!
//! These assert that an ordinary page cannot tell it is being driven by
//! vibesurfer, and that the browser it sees is a possible one. Both
//! halves are ordinary correctness:
//!
//! - **No automation artifacts.** Our shims replace three builtins and
//!   park state on `window`. In 0.2.1 both leaked —
//!   `Function.prototype.toString` returned the download shim's source
//!   and `Object.keys(window)` listed ten `__vs*` globals. A page could
//!   read vibesurfer's own instrumentation.
//! - **No impossible browser.** Running headless left
//!   `document.hasFocus()` false and `window.outerWidth` 0 — states no
//!   real browser reports, which broke `autofocus`, `:focus-visible`,
//!   IME, and any responsive code deriving chrome height from
//!   `outerHeight - innerHeight`.
//!
//! The fixture computes everything itself and puts the verdict in the
//! page text, so the cell asserts on what a page actually observes
//! rather than on a privileged eval.
//!
//! Runs on every backend deliberately. The JS-level fixes are shared by
//! all three, but the window-level ones are per-platform Obj-C /
//! GTK / Win32, so this is the only thing that says whether Linux and
//! Windows have their own versions of the same defects.

use crate::helpers::open_fixture;
use crate::support::{assert_ok, body_rest, each_available_backend, TestContext};

#[test]
fn cell_no_fingerprint_defects() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/fingerprint.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        assert_ok("view", &r);
        let body = body_rest(&r);

        assert!(
            body.contains("FP_OK"),
            "page can observe automation artifacts or an impossible browser.\n\
             Each token after FP_FAIL names the failing check; see \
             fixtures/fingerprint.html for what each asserts.\n\n{body}"
        );
    }
}
