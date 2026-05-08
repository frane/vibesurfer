//! M6 harness smoke test — proves the fixture server, the daemon,
//! and the CLI talk to each other against a real fixture URL.

mod support;

use support::{assert_ok, body_first, each_available_backend, TestContext};

#[test]
fn smoke_open_static_html_against_real_engine() {
    for backend in each_available_backend() {
        eprintln!("== smoke: backend={} ==", backend.name());
        let ctx = TestContext::start();
        let url = ctx.url("/static.html");

        let r = ctx.vs(&["session-open"]);
        assert_ok("session-open", &r);
        let session = body_first(&r);
        assert!(session.starts_with("s_"), "session={session:?}");

        let r = ctx.vs(&["open", &url]);
        assert_ok("open", &r);
        let page = body_first(&r);
        assert!(page.starts_with("p_"), "page={page:?}");

        let r = ctx.vs(&["view", &page]);
        assert_ok("view", &r);
        // A real WebKit-rendered static.html must contain at least our
        // h1 text. The StubEngine never sees this label so a "Stub"
        // result here would prove the harness is talking to the wrong
        // engine.
        assert!(
            r.stdout.contains("Static fixture"),
            "view body should contain h1 text:\n{}",
            r.stdout
        );
    }
}
