//! Isolated sessions: `vs session-open --isolated` gives a session its
//! own in-memory cookie and storage jar. Pages of that session share
//! it; other sessions, isolated or not, never see it.

use crate::support::{assert_ok, body_first, each_available_backend, TestContext};

/// Open an isolated (or default) session and return its id.
fn session(ctx: &TestContext, isolated: bool) -> String {
    let args: &[&str] = if isolated {
        &["session-open", "--isolated"]
    } else {
        &["session-open"]
    };
    let r = ctx.vs(args);
    assert_ok("session-open", &r);
    body_first(&r)
}

/// Open the static fixture in `session`; return the page id.
fn page_in(ctx: &TestContext, session: &str) -> String {
    let r = ctx.vs(&[
        "open",
        &ctx.url("/static.html"),
        &format!("--session={session}"),
    ]);
    assert_ok("open", &r);
    body_first(&r)
}

fn eval_in(ctx: &TestContext, session: &str, page: &str, expr: &str) -> String {
    let r = ctx.vs(&[
        "inspect",
        page,
        "eval",
        expr,
        "--full",
        &format!("--session={session}"),
    ]);
    assert_ok(&format!("eval {expr}"), &r);
    r.stdout
        .lines()
        .find_map(|l| l.strip_prefix("result="))
        .unwrap_or_else(|| panic!("no result= line:\n{}", r.stdout))
        .to_string()
}

/// Store `value` under `key` in both localStorage and a cookie.
fn put(ctx: &TestContext, session: &str, page: &str, key: &str, value: &str) {
    let expr = format!(
        "(() => {{ localStorage.setItem('{key}','{value}'); \
         document.cookie='{key}={value}; path=/'; return 'ok'; }})()"
    );
    eval_in(ctx, session, page, &expr);
}

/// What the page sees under `key`: `"<localStorage>|<cookie>"`, with
/// `null` / empty for absent values.
fn get(ctx: &TestContext, session: &str, page: &str, key: &str) -> String {
    let expr = format!(
        "String(localStorage.getItem('{key}')) + '|' + \
         ((document.cookie.split('; ').find(c => c.startsWith('{key}=')) || '').slice({len}))",
        len = key.len() + 1
    );
    eval_in(ctx, session, page, &expr)
}

fn assert_absent(what: &str, seen: &str) {
    assert!(
        seen.contains("null|") && !seen.contains("|A"),
        "{what}: expected neither localStorage nor cookie, got {seen:?}"
    );
}

fn assert_present(what: &str, seen: &str) {
    assert!(
        seen.contains("A|A"),
        "{what}: expected localStorage and cookie = A, got {seen:?}"
    );
}

// Two isolated sessions on the same origin do not share storage, and a
// default session does not see an isolated one's values either.
#[test]
fn cell_isolated_sessions_do_not_share_storage() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let a = session(&ctx, true);
        let b = session(&ctx, true);
        let key = format!("iso_{a}");
        let pa = page_in(&ctx, &a);
        put(&ctx, &a, &pa, &key, "A");
        assert_present("session A", &get(&ctx, &a, &pa, &key));

        let pb = page_in(&ctx, &b);
        assert_absent("isolated session B", &get(&ctx, &b, &pb, &key));

        let d = session(&ctx, false);
        let pd = page_in(&ctx, &d);
        assert_absent("default session", &get(&ctx, &d, &pd, &key));

        // The other direction: B's writes stay in B.
        put(&ctx, &b, &pb, &key, "B");
        assert_present("session A after B wrote", &get(&ctx, &a, &pa, &key));
    }
}

// Pages of one isolated session share its jar, including a page opened
// after the value was written.
#[test]
fn cell_isolated_session_pages_share_storage() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let a = session(&ctx, true);
        let key = format!("iso_{a}");
        let p1 = page_in(&ctx, &a);
        put(&ctx, &a, &p1, &key, "A");
        let p2 = page_in(&ctx, &a);
        assert_present("second page of session A", &get(&ctx, &a, &p2, &key));
    }
}

// Closing an isolated session discards its jar; the next isolated
// session starts empty.
#[test]
fn cell_isolated_session_starts_empty_after_close() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let a = session(&ctx, true);
        let key = format!("iso_{a}");
        let pa = page_in(&ctx, &a);
        put(&ctx, &a, &pa, &key, "A");
        let r = ctx.vs(&["session-close", &format!("--session={a}")]);
        assert_ok("session-close A", &r);

        let c = session(&ctx, true);
        let pc = page_in(&ctx, &c);
        assert_absent("new isolated session", &get(&ctx, &c, &pc, &key));
    }
}

// `vs status` marks isolated sessions, and only those.
#[test]
fn cell_isolated_session_in_status() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let a = session(&ctx, true);
        let d = session(&ctx, false);
        let row = |id: &str| {
            let r = ctx.vs(&["status", &format!("--session={id}")]);
            assert_ok("status", &r);
            r.stdout
                .lines()
                .find(|l| l.starts_with("session\t") && l.contains(id))
                .unwrap_or_else(|| panic!("no row for {id}:\n{}", r.stdout))
                .to_string()
        };
        assert!(row(&a).ends_with("\tisolated"), "{}", row(&a));
        assert!(!row(&d).contains("isolated"), "{}", row(&d));
    }
}

// `vs auth save/load` work inside isolated sessions: the blob comes
// from A's jar and lands in B's, and nowhere else.
#[test]
fn cell_isolated_session_auth_round_trip() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let a = session(&ctx, true);
        let key = format!("iso_{a}");
        let pa = page_in(&ctx, &a);
        put(&ctx, &a, &pa, &key, "A");
        let r = ctx.vs(&["auth", "save", &pa, "iso-auth", &format!("--session={a}")]);
        assert_ok("auth save in A", &r);

        let b = session(&ctx, true);
        let pb = page_in(&ctx, &b);
        assert_absent("B before load", &get(&ctx, &b, &pb, &key));
        let r = ctx.vs(&["auth", "load", &pb, "iso-auth", &format!("--session={b}")]);
        assert_ok("auth load in B", &r);
        let pb2 = page_in(&ctx, &b);
        assert_present("B after load", &get(&ctx, &b, &pb2, &key));

        let d = session(&ctx, false);
        let pd = page_in(&ctx, &d);
        assert_absent("default session", &get(&ctx, &d, &pd, &key));
    }
}

// The flag survives a daemon restart: the rebuilt page is still
// isolated (its storage is gone, which is documented).
#[test]
fn cell_isolated_session_survives_restart() {
    for _ in each_available_backend() {
        let server = crate::support::FixtureServer::start();
        let home = tempfile::tempdir().unwrap();
        let key = vs_store::MasterKey::generate().unwrap();
        key.write_to_file(home.path().join("key")).unwrap();
        let vs = |args: &[&str]| crate::support::vs(home.path(), args);

        let daemon = crate::support::spawn_daemon(home.path());
        let r = vs(&["session-open", "--isolated"]);
        assert_ok("session-open --isolated", &r);
        let a = body_first(&r);
        let r = vs(&[
            "open",
            &server.url("/static.html"),
            &format!("--session={a}"),
        ]);
        assert_ok("open", &r);
        let page = body_first(&r);

        drop(daemon);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _daemon2 = crate::support::spawn_daemon(home.path());

        // A default session writes to the shared jar; the rebuilt
        // isolated page must not see it.
        let r = vs(&["session-open"]);
        assert_ok("session-open default", &r);
        let d = body_first(&r);
        let r = vs(&[
            "open",
            &server.url("/static.html"),
            &format!("--session={d}"),
        ]);
        assert_ok("open default", &r);
        let pd = body_first(&r);
        let k = format!("iso_{a}");
        let eval = |s: &str, p: &str, e: &str| {
            let r = vs(&["inspect", p, "eval", e, "--full", &format!("--session={s}")]);
            assert_ok("eval", &r);
            r.stdout
        };
        eval(&d, &pd, &format!("localStorage.setItem('{k}','D')"));

        let out = eval(&a, &page, &format!("String(localStorage.getItem('{k}'))"));
        assert!(
            out.contains("result=null"),
            "rebuilt isolated page saw the shared jar:\n{out}"
        );
        let r = vs(&["status", &format!("--session={a}")]);
        assert!(
            r.stdout
                .lines()
                .any(|l| l.starts_with("session\t") && l.contains(&a) && l.ends_with("isolated")),
            "status after restart must keep A isolated:\n{}",
            r.stdout
        );
        // Tidy the shared jar this cell wrote to.
        eval(&d, &pd, &format!("localStorage.removeItem('{k}')"));
    }
}
