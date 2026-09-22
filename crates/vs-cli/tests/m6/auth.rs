//! Auth cells: vs_auth save / load.

use crate::helpers::{eval_js, open_fixture, ref_for};
use crate::support::{
    assert_ok, body_first, body_rest, each_available_backend, token_of, TestContext,
};

// 34. vs_auth save — round-trip cookie.
#[test]
fn cell_auth_save() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/form.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n_email = ref_for(&body, "tf", "");
        let r = ctx.vs(&[
            "act",
            &page,
            &n_email.to_string(),
            "fill",
            "u@example.com",
            &format!("--token={token}"),
        ]);
        let token = token_of(&r);
        let r = ctx.vs(&[
            "act",
            &page,
            &n_email.to_string(),
            "submit",
            &format!("--token={token}"),
        ]);
        assert_ok("submit login", &r);
        let _ = ctx.vs(&["wait", &page, "stable", "--timeout=2000"]);
        let r = ctx.vs(&["auth", "save", &page, "fixture-auth"]);
        assert_ok("auth save", &r);
        let r = ctx.vs(&["auth", "list"]);
        assert_ok("auth list", &r);
        assert!(
            r.stdout.contains("fixture-auth"),
            "auth list should include saved blob:\n{}",
            r.stdout
        );
    }
}

// 35. vs_auth load — apply blob, dashboard renders.
#[test]
fn cell_auth_load() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let (_s, page, _t) = open_fixture(&ctx, "/form.html");
        let r = ctx.vs(&["view", &page, "--full"]);
        let body = body_rest(&r);
        let token = token_of(&r);
        let n_email = ref_for(&body, "tf", "");
        let r = ctx.vs(&[
            "act",
            &page,
            &n_email.to_string(),
            "fill",
            "u@example.com",
            &format!("--token={token}"),
        ]);
        let token = token_of(&r);
        let _ = ctx.vs(&[
            "act",
            &page,
            &n_email.to_string(),
            "submit",
            &format!("--token={token}"),
        ]);
        let _ = ctx.vs(&["wait", &page, "stable", "--timeout=2000"]);
        let r = ctx.vs(&["auth", "save", &page, "fixture-auth"]);
        assert_ok("auth save", &r);
        let r = ctx.vs(&["open", &ctx.url("/dashboard")]);
        assert_ok("open dashboard fresh", &r);
        let new_page = body_first(&r);
        let r = ctx.vs(&["auth", "load", &new_page, "fixture-auth"]);
        assert_ok("auth load", &r);
        let r = ctx.vs(&["open", &ctx.url("/dashboard")]);
        assert_ok("open dashboard after load", &r);
        let p2 = body_first(&r);
        let title = eval_js(&ctx, &p2, "document.title");
        assert!(
            title.contains("Dashboard"),
            "after auth load the dashboard page must render; got {title:?}"
        );
    }
}
// 36. vs_auth save with an HttpOnly session cookie present.
//
// Regression for the v0.1.1 bug: `auth save` used `document.cookie`
// to scrape the cookie jar, which by spec cannot see `HttpOnly`
// cookies. After login on any modern web app the auth cookie was
// silently dropped; the saved blob held only localStorage and any
// non-HttpOnly cookies, and `auth load` had nothing to restore.
//
// v0.1.2 routes the cookie portion through the engine's host-side
// cookie store API (`WKHTTPCookieStore` on macOS), which sees
// `HttpOnly` because the attribute hides the cookie from JS only.
// This cell verifies the save path no longer crashes or no-ops on
// an HttpOnly cookie and that the blob round-trips through load on
// a fresh page. The two-page round-trip is the load-side smoke;
// the deeper proof (HttpOnly preserved bit-for-bit through the
// JSON layer) is the auth.rs unit tests.
#[test]
fn cell_auth_http_only_save_and_load() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let r = ctx.vs(&["session-open"]);
        assert_ok("session-open", &r);

        // Hit the HttpOnly login endpoint. Server returns
        // `Set-Cookie: ht_session=...; HttpOnly`, which lands in the
        // engine's cookie store via the network response parser, not
        // via JS — so `document.cookie` does NOT see it.
        let r = ctx.vs(&["open", &ctx.url("/login-httponly")]);
        assert_ok("open /login-httponly", &r);
        let page = body_first(&r);
        let _ = ctx.vs(&["wait", &page, "stable", "--timeout=3000"]);

        // Save: the host-side path captures ht_session. On v0.1.1
        // this returned success but produced an empty cookies field
        // in the blob.
        let r = ctx.vs(&["auth", "save", &page, "ht-fixture"]);
        assert_ok("auth save with HttpOnly cookie present", &r);
        let r = ctx.vs(&["auth", "list"]);
        assert!(
            r.stdout.contains("ht-fixture"),
            "auth list missing 'ht-fixture' after save:\n{}",
            r.stdout
        );

        // Load on a fresh page in the same session. The cookie is
        // already in the engine's process-wide store (so this
        // doesn't prove load alone restored it — that's covered by
        // the auth.rs JSON round-trip unit tests), but it does
        // confirm the load call accepts the new structured blob
        // shape and applies cookies without error.
        let r = ctx.vs(&["open", &ctx.url("/dashboard-httponly")]);
        let dash = body_first(&r);
        let r = ctx.vs(&["auth", "load", &dash, "ht-fixture"]);
        assert_ok("auth load", &r);
        let r = ctx.vs(&["open", &ctx.url("/dashboard-httponly")]);
        let after = body_first(&r);
        let r = ctx.vs(&["view", &after, "--full"]);
        let body = body_rest(&r);
        assert!(
            !body.contains("unauthenticated"),
            "dashboard 401 after auth load + nav; cookie not in store:\n{body}"
        );
    }
}

// vs_auth import — the passkey fallback. A human logs in with a passkey
// in their own browser, exports cookies + storage as a v2 auth-blob
// JSON, imports it here, and loads it onto a headless page.
#[test]
fn cell_auth_import_and_load() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let blob = serde_json::json!({
            "version": 2,
            "url": "http://127.0.0.1/",
            "origin": "http://127.0.0.1",
            "cookies": [{"name": "sid", "value": "abc", "domain": "127.0.0.1", "path": "/"}],
            "localStorage": {"k": "v"},
            "sessionStorage": {}
        })
        .to_string();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(&path, blob).unwrap();

        let r = ctx.vs(&["auth", "import", "imported", path.to_str().unwrap()]);
        assert_ok("auth import", &r);
        let r = ctx.vs(&["auth", "list"]);
        assert!(
            r.stdout.contains("imported"),
            "auth list should include the imported blob:\n{}",
            r.stdout
        );
        // Loading it onto a page injects the cookies + storage.
        let (_s, page, _t) = open_fixture(&ctx, "/form.html");
        let r = ctx.vs(&["auth", "load", &page, "imported"]);
        assert!(
            r.stdout.contains("auth_loaded"),
            "loading the imported blob should succeed:\n{}",
            r.stdout
        );
    }
}

// vs_auth webauthn — the virtual authenticator. A pure-JS software
// authenticator overrides navigator.credentials so a real passkey
// registration + login round-trips headlessly. The fixture registers a
// credential, authenticates, and verifies the assertion signature
// against the registered public key with WebCrypto; VERIFIED proves the
// authenticator is cryptographically correct.
// The virtual WebAuthn authenticator is installed via the Cocoa
// backend's document-start user script; wpe/webview2 don't yet, so this
// cell is macOS-only for now.
#[cfg(target_os = "macos")]
#[test]
fn cell_auth_webauthn_virtual_authenticator() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        // Addressed by name, not by IP. WebAuthn's relying-party id
        // must be a registrable domain and an IP literal is not one;
        // WebKit used to allow `127.0.0.1` and stopped (macOS 27 fails
        // the create() outright with "the effective domain of the
        // document is not a valid domain"). This cell was skipped on
        // CI for a year, blamed on the runner's `crypto.subtle` never
        // completing — the flow never got as far as crypto.subtle.
        //
        // Load any page, enable the authenticator (installs a
        // document-start shim), then navigate to the WebAuthn fixture so
        // the shim is in place before its create()/get() run.
        let r = ctx.vs(&["session-open"]);
        assert_ok("session-open", &r);
        let r = ctx.vs(&["open", &ctx.url_named_host("/static.html")]);
        assert_ok("open", &r);
        let page = body_first(&r);
        let r = ctx.vs(&["auth", "webauthn", &page]);
        assert_ok("auth webauthn", &r);
        let r = ctx.vs(&["goto", &page, &ctx.url_named_host("/webauthn.html")]);
        assert_ok("goto webauthn fixture", &r);
        // 20s: WebCrypto ES256 sign+verify plus the wait poll is slower
        // on CI runners than locally; 8s flaked on the macOS runner.
        let r = ctx.vs(&["wait", &page, "text", "VERIFIED", "--timeout=20000"]);
        assert_ok("wait for VERIFIED", &r);
        let status = eval_js(&ctx, &page, "document.getElementById('status').textContent");
        assert!(
            status.contains("VERIFIED"),
            "virtual authenticator: create->get->verify must round-trip, got {status:?}"
        );
    }
}

/// 36b. `vs auth save` / `load` carry IndexedDB.
///
/// The blob held cookies plus local and session storage, and nothing
/// else. A site that keeps its session in IndexedDB — a local-first
/// store, a Firebase or Supabase client — restored as logged out on a
/// blob that looked complete, which is the worst shape for this to
/// fail in: `auth load` said ok and the next call acted as though it
/// had a session. The storage fixture writes one record into
/// `vibesurfer_demo_db`; it has to come back on a page that never saw
/// the fixture's script.
#[test]
fn cell_auth_carries_indexeddb() {
    for _ in each_available_backend() {
        let ctx = TestContext::start_with_env(&[("VS_IDB_TRACE", "1")]);
        let (_s, page, _t) = open_fixture(&ctx, "/storage.html");
        // The fixture's write is async; it flags itself when done.
        // Wait on that and prove the record is really there, because a
        // save that runs first captures an origin with no databases
        // and every later step then fails somewhere else entirely.
        assert_ok(
            "wait for the fixture's write",
            &ctx.vs(&["wait", &page, "text", "Storage ready.", "--timeout=15000"]),
        );
        let written = read_idb_item(&ctx, &page);
        assert!(
            written.contains("first item"),
            "fixture must have written the record before the save, got {written:?}"
        );

        let r = ctx.vs(&["auth", "save", &page, "idb-fixture"]);
        assert_ok("auth save", &r);
        // A blob that quietly left the databases out is the failure
        // this cell exists to catch, and it says so on the way out.
        assert!(
            !r.stdout.contains("storage_partial"),
            "the save must carry the database, got {:?}",
            r.stdout
        );

        // IndexedDB is per-origin and the fixture server is one
        // origin, so a second page would simply still see the
        // database. Close the writer (it holds a connection, which
        // blocks a delete) and wipe it: what comes back after that can
        // only have come back through the blob.
        assert_ok("close writer", &ctx.vs(&["close", &page]));
        let r = ctx.vs(&["open", &ctx.url("/static.html")]);
        assert_ok("open plain page", &r);
        let plain = body_first(&r);
        delete_idb(&ctx, &plain);
        let before = read_idb_item(&ctx, &plain);
        assert!(
            before.contains("none"),
            "the database must be gone before the load, got {before:?}"
        );

        let r = ctx.vs(&["auth", "load", &plain, "idb-fixture"]);
        assert_ok("auth load", &r);
        // Each read opens its own connection, and on a loaded machine
        // a freshly committed write is not always visible to the next
        // one immediately, so give it a few attempts before calling it
        // a lost record.
        let mut after = String::new();
        for _ in 0..10 {
            after = read_idb_item(&ctx, &plain);
            if after.contains("first item") {
                break;
            }
        }
        assert!(
            after.contains("first item"),
            "the restored record must be readable, got {after:?}; load said {:?}; origin has {}; daemon log: {}",
            r.stdout,
            describe_idb(&ctx, &plain),
            std::fs::read_to_string(ctx.home_path().join("daemon.log"))
                .unwrap_or_default()
                .lines()
                .filter(|l| l.contains("idb"))
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
}

/// Delete the fixture's database and wait for the delete to land.
fn delete_idb(ctx: &TestContext, page: &str) {
    let out = poll_idb(
        ctx,
        page,
        "del",
        "\
          var rq = indexedDB.deleteDatabase('vibesurfer_demo_db');\
          rq.onsuccess = function(){ done('gone'); };\
          rq.onerror = function(){ done('gone'); };\
          rq.onblocked = function(){ done('blocked'); };",
    );
    assert!(
        !out.contains("blocked"),
        "delete blocked by a live connection"
    );
}

/// Read `items` key 1 out of the fixture's database. `open` with no
/// version would create the database if it were missing, so a missing
/// store reports "none" rather than pretending.
fn read_idb_item(ctx: &TestContext, page: &str) -> String {
    poll_idb(ctx, page, "read", "\
          var rq = indexedDB.open('vibesurfer_demo_db');\
          rq.onerror = function(){ done('none'); };\
          rq.onsuccess = function(){\
            var db = rq.result;\
            if (!db.objectStoreNames.contains('items')) { done('none'); db.close(); return; }\
            var g = db.transaction('items','readonly').objectStore('items').get(1);\
            g.onsuccess = function(){ done(g.result ? JSON.stringify(g.result) : 'none'); db.close(); };\
            g.onerror = function(){ done('none'); db.close(); };\
          };")
}

/// Run an async IndexedDB snippet on the page and poll for its answer.
///
/// The engines cannot await inside one eval, so the snippet parks its
/// result on a page global and later evals read it. Each call gets its
/// own global: sharing one meant a later read could pick up an earlier
/// call's answer, which reads exactly like a restore that did not
/// happen.
fn poll_idb(ctx: &TestContext, page: &str, kind: &str, snippet: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let slot = format!("__vsProbe_{kind}_{}", SEQ.fetch_add(1, Ordering::Relaxed));
    let js = format!(
        "(function(){{\
            if (!window.{slot}) {{\
              window.{slot} = 'pending';\
              var done = function(v){{ window.{slot} = v; }};\
              {snippet}\
            }}\
            return window.{slot};\
        }})()"
    );
    for _ in 0..40 {
        let out = eval_js(ctx, page, &js);
        if !out.contains("pending") {
            return out;
        }
    }
    "pending".into()
}

/// What the origin actually holds, for a failure message: database
/// names, versions, and each one's object stores.
fn describe_idb(ctx: &TestContext, page: &str) -> String {
    poll_idb(
        ctx,
        page,
        "desc",
        "\
          indexedDB.databases().then(function(list){\
            var out = [], left = list.length;\
            if (!left) { done('no databases'); return; }\
            list.forEach(function(d){\
              var rq = indexedDB.open(d.name);\
              rq.onsuccess = function(){\
                out.push(d.name + '@v' + rq.result.version + ' stores=' + \
                  Array.prototype.slice.call(rq.result.objectStoreNames).join(','));\
                rq.result.close();\
                if (--left === 0) done(out.join(' | '));\
              };\
              rq.onerror = function(){ if (--left === 0) done(out.join(' | ')); };\
            });\
          }).catch(function(e){ done('databases() failed: ' + e); });",
    )
}
