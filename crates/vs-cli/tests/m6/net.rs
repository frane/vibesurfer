//! Request interception: `vs session-open --intercept` routes a
//! session's pages through the daemon's HTTP proxy, and `vs net`
//! observes and routes every request (documents, subresources,
//! iframes), not just fetch/XHR.
//!
//! The fixture is addressed as `vs.localhost`: WebKit on macOS never
//! proxies `127.0.0.1` or `localhost`, but does proxy `*.localhost`
//! names, which the proxy resolves to loopback.

use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;

use crate::support::hit_server::HitServer;
use crate::support::{assert_err, assert_ok, body_first, each_available_backend, vs, TestContext};

/// One parsed `vs net poll` event.
#[derive(Debug, Clone)]
struct Ev {
    /// `req`, `res`, `fin`, `fail`, `drop`.
    kind: String,
    id: u64,
    /// The rest of the event's first line after kind and id.
    line: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

impl Ev {
    fn url(&self) -> &str {
        // req: <METHOD> <url> ...; res: <status> <url> ...
        self.line.split(' ').nth(1).unwrap_or("")
    }
    fn field(&self, key: &str) -> Option<&str> {
        self.line
            .split(' ')
            .find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
    }
}

fn parse_events(body: &str) -> Vec<Ev> {
    let mut out: Vec<Ev> = Vec::new();
    for line in body.lines() {
        if let Some(h) = line.strip_prefix("h ") {
            let (k, v) = h.split_once(": ").unwrap_or((h, ""));
            out.last_mut()
                .expect("header line before any event")
                .headers
                .push((k.to_string(), v.to_string()));
            continue;
        }
        if let Some(b) = line.strip_prefix("body ") {
            out.last_mut().expect("body before event").body = Some(B64.decode(b).unwrap());
            continue;
        }
        let mut it = line.splitn(3, ' ');
        let kind = it.next().unwrap_or("").to_string();
        if kind.is_empty() {
            continue;
        }
        let id = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        out.push(Ev {
            kind,
            id,
            line: it.next().unwrap_or("").to_string(),
            headers: Vec::new(),
            body: None,
        });
    }
    out
}

/// Proxied fixture URL (see the module docs).
fn purl(ctx: &TestContext, path: &str) -> String {
    ctx.url(path).replace("//127.0.0.1:", "//vs.localhost:")
}

/// The fixture URL on 127.0.0.1 for `loopback`, else under a name.
fn hurl(ctx: &TestContext, loopback: bool, path: &str) -> String {
    if loopback {
        ctx.url(path)
    } else {
        purl(ctx, path)
    }
}

fn intercept_session(ctx: &TestContext) -> String {
    let r = ctx.vs(&["session-open", "--intercept"]);
    assert_ok("session-open --intercept", &r);
    body_first(&r)
}

fn sv(ctx: &TestContext, s: &str, args: &[&str]) -> crate::support::CliOut {
    let mut a: Vec<&str> = args.to_vec();
    let flag = format!("--session={s}");
    a.push(&flag);
    ctx.vs(&a)
}

fn poll(ctx: &TestContext, s: &str, wait_ms: u64) -> Vec<Ev> {
    let r = sv(ctx, s, &["net", "poll", "--wait-ms", &wait_ms.to_string()]);
    assert_ok("net poll", &r);
    let body: String = r.stdout.lines().skip(1).collect::<Vec<_>>().join("\n");
    parse_events(&body)
}

fn eval(ctx: &TestContext, s: &str, page: &str, expr: &str) -> String {
    let r = sv(ctx, s, &["inspect", page, "eval", expr, "--full"]);
    assert_ok(&format!("eval {expr}"), &r);
    r.stdout
        .lines()
        .find_map(|l| l.strip_prefix("result="))
        .unwrap_or_else(|| panic!("no result= line:\n{}", r.stdout))
        .to_string()
}

/// Poll until `done` says so or `budget` runs out, answering every
/// held request with `decide` (`None` continues it unchanged). Returns
/// all events seen. Only call with routing on: every `req` is held.
fn drive(
    ctx: &TestContext,
    s: &str,
    budget: Duration,
    mut decide: impl FnMut(&Ev) -> Option<Vec<String>>,
    mut done: impl FnMut(&[Ev]) -> bool,
) -> Vec<Ev> {
    let start = Instant::now();
    let mut seen = Vec::new();
    while start.elapsed() < budget {
        for ev in poll(ctx, s, 500) {
            if ev.kind == "req" {
                let id = ev.id.to_string();
                let args = decide(&ev).unwrap_or_else(|| vec!["continue".into()]);
                let mut a: Vec<&str> = vec!["net"];
                a.push(&args[0]);
                a.push(&id);
                a.extend(args[1..].iter().map(String::as_str));
                let r = sv(ctx, s, &a);
                // The browser may give up on a request before the
                // decision lands (WebView2 drops its own background
                // requests); the proxy then answers NOT_FOUND. A
                // request the test cares about is checked below.
                if r.stdout.starts_with("! NOT_FOUND") {
                    eprintln!("drive: {} {} already gone", ev.id, ev.url());
                    seen.push(ev);
                    continue;
                }
                assert_ok(&format!("net {args:?} {id}"), &r);
            }
            seen.push(ev);
        }
        if done(&seen) {
            return seen;
        }
    }
    panic!("drive: condition not met within {budget:?}; events: {seen:#?}");
}

/// Wait for `eval` to return something containing `want`.
fn eventually(ctx: &TestContext, s: &str, page: &str, expr: &str, want: &str) -> String {
    let start = Instant::now();
    loop {
        let got = eval(ctx, s, page, expr);
        if got.contains(want) || start.elapsed() > Duration::from_secs(10) {
            return got;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Run `vs goto` on another thread: with routing on it blocks until
/// the test answers the document request.
fn goto_async(ctx: &TestContext, s: &str, page: &str, url: &str) -> std::thread::JoinHandle<()> {
    let home = ctx.home_path().to_path_buf();
    let (s, page, url) = (s.to_string(), page.to_string(), url.to_string());
    std::thread::spawn(move || {
        let r = vs(&home, &["goto", &page, &url, &format!("--session={s}")]);
        assert_ok("goto", &r);
    })
}

fn open_blank(ctx: &TestContext, s: &str) -> String {
    let r = sv(ctx, s, &["open", "about:blank"]);
    assert_ok("open about:blank", &r);
    body_first(&r)
}

fn find<'a>(evs: &'a [Ev], kind: &str, url_end: &str) -> Option<&'a Ev> {
    evs.iter()
        .find(|e| e.kind == kind && e.url().ends_with(url_end))
}

/// How many requests whose URL ends with `url_end` have finished. The
/// browser's own traffic (WebView2 reaches SmartScreen through the
/// proxy too) finishes on its own schedule and must not count.
fn fins(evs: &[Ev], url_end: &str) -> usize {
    evs.iter()
        .filter(|f| f.kind == "fin")
        .filter(|f| {
            evs.iter()
                .any(|r| r.kind == "req" && r.id == f.id && r.url().ends_with(url_end))
        })
        .count()
}

fn status_of(evs: &[Ev], id: u64) -> Option<String> {
    evs.iter()
        .find(|e| e.kind == "res" && e.id == id)
        .map(|e| e.line.split(' ').next().unwrap_or("").to_string())
}

// (a) A navigation, its script and its image each show up as their own
// req/res/fin, with the document marked as a navigation.
#[test]
fn cell_intercept_observes_document_and_subresources() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let s = intercept_session(&ctx);
        let r = sv(&ctx, &s, &["open", &purl(&ctx, "/net.html")]);
        assert_ok("open", &r);
        let page = body_first(&r);
        assert_eq!(
            eventually(&ctx, &s, &page, "String(window.__img)", "ok"),
            "ok"
        );

        // Routing is off: nothing is held, the events are already queued.
        let evs = poll(&ctx, &s, 0);
        let doc = find(&evs, "req", "/net.html").expect("document req");
        assert!(doc.line.starts_with("GET "), "{doc:?}");
        assert_eq!(doc.field("type"), Some("document"), "{doc:?}");
        assert_eq!(doc.field("nav"), Some("1"), "{doc:?}");
        assert_eq!(doc.field("frame"), Some("main"), "{doc:?}");
        assert_eq!(status_of(&evs, doc.id).as_deref(), Some("200"));
        let script = find(&evs, "req", "/static/script-a.js").expect("script req");
        assert_eq!(script.field("type"), Some("script"), "{script:?}");
        assert_eq!(script.field("nav"), Some("0"), "{script:?}");
        assert_eq!(status_of(&evs, script.id).as_deref(), Some("200"));
        let img = find(&evs, "req", "/img/pixel.gif").expect("image req");
        assert_eq!(img.field("type"), Some("image"), "{img:?}");
        assert_eq!(status_of(&evs, img.id).as_deref(), Some("200"));

        // The document's body is kept for `vs net body`.
        let r = sv(&ctx, &s, &["net", "body", &doc.id.to_string()]);
        assert_ok("net body", &r);
        let b64 = body_first(&r);
        let html = B64.decode(b64.strip_prefix("body ").unwrap()).unwrap();
        assert!(String::from_utf8_lossy(&html).contains("Net fixture"));
    }
}

// (b) With routing on, a navigation can be answered by the client.
#[test]
fn cell_intercept_fulfills_a_navigation() {
    for (_, loopback) in each_available_backend().flat_map(|b| [(b, false), (b, true)]) {
        let ctx = TestContext::start();
        let s = intercept_session(&ctx);
        let page = open_blank(&ctx, &s);
        assert_ok("route on", &sv(&ctx, &s, &["net", "route", "on"]));
        let url = hurl(&ctx, loopback, "/does-not-exist.html");
        let nav = goto_async(&ctx, &s, &page, &url);
        let html = B64.encode("<!doctype html><title>t</title><h1 id=x>fulfilled by client</h1>");
        drive(
            &ctx,
            &s,
            Duration::from_secs(20),
            |ev| {
                ev.url().ends_with("/does-not-exist.html").then(|| {
                    vec![
                        "fulfill".into(),
                        "--status".into(),
                        "200".into(),
                        "--header".into(),
                        "Content-Type: text/html".into(),
                        "--body-b64".into(),
                        html.clone(),
                    ]
                })
            },
            |e| fins(e, "/does-not-exist.html") >= 1 && nav.is_finished(),
        );
        nav.join().unwrap();
        assert_ok("route off", &sv(&ctx, &s, &["net", "route", "off"]));
        let text = eventually(
            &ctx,
            &s,
            &page,
            "document.body.innerText",
            "fulfilled by client",
        );
        assert!(text.contains("fulfilled by client"), "page text: {text}");
    }
}

// (c) Aborting an image fails it for the page and reports `fail`.
#[test]
fn cell_intercept_aborts_an_image() {
    for (_, loopback) in each_available_backend().flat_map(|b| [(b, false), (b, true)]) {
        let ctx = TestContext::start();
        let s = intercept_session(&ctx);
        let page = open_blank(&ctx, &s);
        assert_ok("route on", &sv(&ctx, &s, &["net", "route", "on"]));
        let nav = goto_async(&ctx, &s, &page, &hurl(&ctx, loopback, "/net.html"));
        let evs = drive(
            &ctx,
            &s,
            Duration::from_secs(20),
            |ev| {
                ev.url()
                    .ends_with("/img/pixel.gif")
                    .then(|| vec!["abort".into(), "--error".into(), "blockedbyclient".into()])
            },
            |e| e.iter().any(|x| x.kind == "fail") && nav.is_finished(),
        );
        nav.join().unwrap();
        let img = find(&evs, "req", "/img/pixel.gif").expect("image req");
        let fail = evs
            .iter()
            .find(|e| e.kind == "fail" && e.id == img.id)
            .expect("fail line for the image");
        assert_eq!(fail.line, "blockedbyclient");
        assert_eq!(
            eventually(&ctx, &s, &page, "String(window.__img)", "error"),
            "error"
        );
        assert_ok("route off", &sv(&ctx, &s, &["net", "route", "off"]));
    }
}

// (d) `continue` with a changed header list reaches the server.
#[test]
fn cell_intercept_continue_with_header() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let s = intercept_session(&ctx);
        let r = sv(&ctx, &s, &["open", &purl(&ctx, "/net.html")]);
        assert_ok("open", &r);
        let page = body_first(&r);
        poll(&ctx, &s, 0);
        assert_ok("route on", &sv(&ctx, &s, &["net", "route", "on"]));
        eval(
            &ctx,
            &s,
            &page,
            "(fetch('/echo-headers').then(r => r.text()).then(t => window.__eh = t), 'started')",
        );
        drive(
            &ctx,
            &s,
            Duration::from_secs(20),
            |ev| {
                ev.url().ends_with("/echo-headers").then(|| {
                    let mut args = vec!["continue".to_string()];
                    for (k, v) in &ev.headers {
                        args.push("--header".into());
                        args.push(format!("{k}: {v}"));
                    }
                    args.push("--header".into());
                    args.push("X-Injected: by-client".into());
                    args
                })
            },
            |e| fins(e, "/echo-headers") >= 1,
        );
        let seen = eventually(
            &ctx,
            &s,
            &page,
            "String(window.__eh).replace(/\\n/g, ' | ')",
            "by-client",
        );
        assert!(
            seen.to_ascii_lowercase().contains("x-injected: by-client"),
            "server saw: {seen}"
        );
        assert_ok("route off", &sv(&ctx, &s, &["net", "route", "off"]));
    }
}

// (e) `fetch` returns the real response without delivering it; the
// client then fulfills with a changed body.
#[test]
fn cell_intercept_fetch_then_fulfill() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let s = intercept_session(&ctx);
        let r = sv(&ctx, &s, &["open", &purl(&ctx, "/net.html")]);
        assert_ok("open", &r);
        let page = body_first(&r);
        poll(&ctx, &s, 0);
        assert_ok("route on", &sv(&ctx, &s, &["net", "route", "on"]));
        eval(
            &ctx,
            &s,
            &page,
            "(fetch('/api/ok').then(r => r.text()).then(t => window.__ok = t), 'started')",
        );
        drive(
            &ctx,
            &s,
            Duration::from_secs(20),
            |ev| {
                if !ev.url().ends_with("/api/ok") {
                    return None;
                }
                let r = sv(&ctx, &s, &["net", "fetch", &ev.id.to_string()]);
                assert_ok("net fetch", &r);
                let body: String = r.stdout.lines().skip(1).collect::<Vec<_>>().join("\n");
                let fetched = parse_events(&body);
                let res = &fetched[0];
                assert_eq!(res.kind, "res");
                assert!(res.line.starts_with("200 "), "{res:?}");
                let real = String::from_utf8(res.body.clone().unwrap()).unwrap();
                assert_eq!(real, r#"{"ok":true}"#);
                let patched = real.replace("true", "\"patched\"");
                Some(vec![
                    "fulfill".into(),
                    "--header".into(),
                    "Content-Type: application/json".into(),
                    "--body-b64".into(),
                    B64.encode(patched),
                ])
            },
            |e| fins(e, "/api/ok") >= 1,
        );
        let got = eventually(&ctx, &s, &page, "String(window.__ok)", "patched");
        assert!(got.contains(r#"{"ok":"patched"}"#), "page got: {got}");
        assert_ok("route off", &sv(&ctx, &s, &["net", "route", "off"]));
    }
}

// (f) An iframe's document request is routed like any other.
#[test]
fn cell_intercept_fulfills_an_iframe() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let s = intercept_session(&ctx);
        let page = open_blank(&ctx, &s);
        assert_ok("route on", &sv(&ctx, &s, &["net", "route", "on"]));
        let nav = goto_async(&ctx, &s, &page, &purl(&ctx, "/net-frame.html"));
        let inner = B64.encode("<!doctype html><p id=inner>inner from client</p>");
        let evs = drive(
            &ctx,
            &s,
            Duration::from_secs(20),
            |ev| {
                ev.url().ends_with("/net/inner.html").then(|| {
                    vec![
                        "fulfill".into(),
                        "--header".into(),
                        "Content-Type: text/html".into(),
                        "--body-b64".into(),
                        inner.clone(),
                    ]
                })
            },
            |e| {
                find(e, "req", "/net/inner.html")
                    .is_some_and(|r| e.iter().any(|x| x.kind == "fin" && x.id == r.id))
                    && nav.is_finished()
            },
        );
        nav.join().unwrap();
        let frame = find(&evs, "req", "/net/inner.html").unwrap();
        assert_eq!(frame.field("type"), Some("document"), "{frame:?}");
        assert_eq!(frame.field("frame"), Some("sub"), "{frame:?}");
        let text = eventually(
            &ctx,
            &s,
            &page,
            "String(document.getElementById('f').contentDocument.body.innerText)",
            "inner from client",
        );
        assert!(text.contains("inner from client"), "iframe text: {text}");
        assert_ok("route off", &sv(&ctx, &s, &["net", "route", "off"]));
    }
}

// (g) Sessions without --intercept load directly and have no `vs net`.
#[test]
fn cell_intercept_off_by_default() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let s = intercept_session(&ctx);
        let r = ctx.vs(&["session-open"]);
        assert_ok("session-open", &r);
        let plain = body_first(&r);
        let r = sv(&ctx, &plain, &["open", &ctx.url("/net.html")]);
        assert_ok("open", &r);
        let page = body_first(&r);
        assert_eq!(
            eventually(&ctx, &plain, &page, "String(window.__img)", "ok"),
            "ok"
        );
        let err = assert_err(&sv(&ctx, &plain, &["net", "poll"]));
        assert_eq!(err, "BAD_REQUEST");
        // The intercepting session's proxy saw none of it.
        assert!(poll(&ctx, &s, 0).is_empty());
    }
}

// (g2) A page served from 127.0.0.1 is proxied too. WebKitGTK (empty
// ignore list) and WebView2 (`<-loopback>`) proxy it like any other
// host; WebKit on macOS never proxies loopback, so such a page gets a
// web view whose http(s) goes to the proxy through a scheme handler.
// Either way the document, its script and its image are seen, and a
// later navigation to another host still is.
#[test]
fn cell_intercept_loopback_by_engine() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let s = intercept_session(&ctx);
        let page = open_blank(&ctx, &s);
        let r = sv(&ctx, &s, &["goto", &page, &ctx.url("/net.html")]);
        assert_ok("goto", &r);
        assert_eq!(
            eventually(&ctx, &s, &page, "String(window.__img)", "ok"),
            "ok"
        );
        let evs = poll(&ctx, &s, 0);
        let doc = find(&evs, "req", "/net.html").expect("127.0.0.1 was not proxied");
        assert_eq!(doc.field("type"), Some("document"), "{doc:?}");
        assert_eq!(doc.field("nav"), Some("1"), "{doc:?}");
        assert_eq!(status_of(&evs, doc.id).as_deref(), Some("200"));
        let script = find(&evs, "req", "/static/script-a.js").expect("script req");
        assert_eq!(script.field("type"), Some("script"), "{script:?}");
        let img = find(&evs, "req", "/img/pixel.gif").expect("image req");
        assert_eq!(img.field("type"), Some("image"), "{img:?}");

        // Over to a non-loopback name and back: both seen.
        let r = sv(&ctx, &s, &["goto", &page, &purl(&ctx, "/net.html")]);
        assert_ok("goto vs.localhost", &r);
        let r = sv(&ctx, &s, &["goto", &page, &ctx.url("/net.html")]);
        assert_ok("goto 127.0.0.1", &r);
        assert_eq!(
            eventually(&ctx, &s, &page, "String(window.__img)", "ok"),
            "ok"
        );
        let evs = poll(&ctx, &s, 0);
        let docs: Vec<&Ev> = evs
            .iter()
            .filter(|e| e.kind == "req" && e.url().ends_with("/net.html"))
            .collect();
        assert_eq!(docs.len(), 2, "{evs:#?}");
        assert!(docs[0].url().contains("vs.localhost"), "{docs:?}");
        assert!(docs[1].url().contains("127.0.0.1"), "{docs:?}");
    }
}

// (g3) Cookies and redirects on a loopback page: a Set-Cookie lands in
// the jar (document.cookie sees it, the next request sends it), and a
// redirect chain ends on the final URL with each hop reported.
#[test]
fn cell_intercept_loopback_cookies_and_redirects() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let s = intercept_session(&ctx);
        let page = open_blank(&ctx, &s);
        let r = sv(&ctx, &s, &["goto", &page, &ctx.url("/redirect-chain/3")]);
        assert_ok("goto redirect chain", &r);
        let href = eval(&ctx, &s, &page, "location.pathname");
        assert!(href.contains("/net.html"), "landed on {href}");
        let evs = poll(&ctx, &s, 0);
        let hops = evs
            .iter()
            .filter(|e| e.kind == "req" && e.url().contains("/redirect-chain/"))
            .count();
        assert_eq!(hops, 3, "{evs:#?}");

        eval(
            &ctx,
            &s,
            &page,
            "fetch('/set-cookie?n=vs_loop&v=1').then(() => fetch('/echo-headers')).then(r => r.text()).then(t => { window.__echo = t; }), 1",
        );
        let got = eventually(&ctx, &s, &page, "document.cookie", "vs_loop=1");
        assert!(got.contains("vs_loop=1"), "document.cookie: {got}");
        let echoed = eventually(
            &ctx,
            &s,
            &page,
            "JSON.stringify(String(window.__echo))",
            "vs_loop=1",
        );
        assert!(echoed.contains("vs_loop=1"), "request headers: {echoed}");
    }
}

// (h) The Playwright case: a page fetches an https URL whose host does
// not exist, and the client fulfills it. Upstream is never touched.
#[test]
fn cell_intercept_fulfills_https_without_upstream() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let s = intercept_session(&ctx);
        let r = sv(&ctx, &s, &["open", &purl(&ctx, "/net.html")]);
        assert_ok("open", &r);
        let page = body_first(&r);
        poll(&ctx, &s, 0);
        assert_ok("route on", &sv(&ctx, &s, &["net", "route", "on"]));
        eval(
            &ctx,
            &s,
            &page,
            "(fetch('https://example.invalid/cars').then(r => r.json()).then(j => window.__cars = JSON.stringify(j), e => window.__cars = 'ERR ' + e), 'started')",
        );
        let json = B64.encode(r#"[{"name":"Tesla"}]"#);
        let evs = drive(
            &ctx,
            &s,
            Duration::from_secs(20),
            |ev| {
                (ev.url() == "https://example.invalid/cars").then(|| {
                    vec![
                        "fulfill".into(),
                        "--header".into(),
                        "Content-Type: application/json".into(),
                        "--header".into(),
                        "Access-Control-Allow-Origin: *".into(),
                        "--body-b64".into(),
                        json.clone(),
                    ]
                })
            },
            |e| fins(e, "/cars") >= 1,
        );
        assert!(find(&evs, "req", "https://example.invalid/cars").is_some());
        let got = eventually(&ctx, &s, &page, "String(window.__cars)", "Tesla");
        assert_eq!(got, r#"[{"name":"Tesla"}]"#);
        assert_ok("route off", &sv(&ctx, &s, &["net", "route", "off"]));
    }
}

// (i) A real https response through the proxy: TLS to the browser with
// the daemon's CA, verified TLS upstream (the test upstream's CA comes
// from VS_INTERCEPT_EXTRA_ROOTS).
#[test]
fn cell_intercept_records_a_real_https_response() {
    for _ in each_available_backend() {
        let up = HitServer::tls();
        let roots = up.ca_pem_path().to_string_lossy().to_string();
        let ctx = TestContext::start_with_env(&[("VS_INTERCEPT_EXTRA_ROOTS", &roots)]);
        let s = intercept_session(&ctx);
        let r = sv(&ctx, &s, &["open", &purl(&ctx, "/net.html")]);
        assert_ok("open", &r);
        let page = body_first(&r);
        let url = up.url("/hit");
        eval(
            &ctx,
            &s,
            &page,
            &format!(
                "(fetch('{url}').then(r => r.text()).then(t => window.__hit = 'got ' + t, e => window.__hit = 'ERR ' + e), 'started')"
            ),
        );
        let got = eventually(&ctx, &s, &page, "String(window.__hit)", "got");
        assert_eq!(got, "got 1");
        let evs = poll(&ctx, &s, 0);
        let req = find(&evs, "req", "/hit").expect("https req");
        assert_eq!(req.url(), url, "{req:?}");
        assert_eq!(status_of(&evs, req.id).as_deref(), Some("200"));
        assert_eq!(up.hits(), 1);
    }
}

// (j) Abort is final: the browser must not retry an aborted request
// (CFNetwork retries an idempotent request whose connection closed
// with no bytes). Three equal fetches, continue / abort / continue:
// the page sees ok, failure, ok and the server sees two hits. Over
// plain http and over https.
#[test]
fn cell_intercept_abort_is_not_retried() {
    for (_, loopback) in each_available_backend().flat_map(|b| [(b, false), (b, true)]) {
        let tls = HitServer::tls();
        let plain = HitServer::plain();
        let roots = tls.ca_pem_path().to_string_lossy().to_string();
        let ctx = TestContext::start_with_env(&[("VS_INTERCEPT_EXTRA_ROOTS", &roots)]);
        let s = intercept_session(&ctx);
        let r = sv(&ctx, &s, &["open", &hurl(&ctx, loopback, "/net.html")]);
        assert_ok("open", &r);
        let page = body_first(&r);
        for up in [&plain, &tls] {
            let url = up.url("/zzz");
            poll(&ctx, &s, 0);
            assert_ok("route on", &sv(&ctx, &s, &["net", "route", "on"]));
            eval(
                &ctx,
                &s,
                &page,
                &format!(
                    "(window.__eq = null, (async () => {{ const out = []; for (let i = 0; i < 3; i++) {{ out.push(await fetch('{url}').then(r => r.text().then(t => t, () => 'BODYFAILED'), () => 'FAILED')); }} window.__eq = out.join(','); }})(), 'started')"
                ),
            );
            let mut n = 0;
            let evs = drive(
                &ctx,
                &s,
                Duration::from_secs(30),
                |ev| {
                    if !ev.url().ends_with("/zzz") {
                        return None;
                    }
                    n += 1;
                    (n == 2).then(|| vec!["abort".into(), "--error".into(), "failed".into()])
                },
                |e| fins(e, "/zzz") >= 2,
            );
            let got = eventually(&ctx, &s, &page, "String(window.__eq)", ",");
            assert_ok("route off", &sv(&ctx, &s, &["net", "route", "off"]));
            let lines: Vec<String> = evs
                .iter()
                .map(|e| format!("{} {} {}", e.kind, e.id, e.line))
                .collect();
            assert_eq!(got, "1,FAILED,2", "{url}: {lines:#?}");
            assert_eq!(up.hits(), 2, "{url}");
            assert_eq!(n, 3, "{url}: requests seen");
        }
    }
}

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
            assert_ok("net dialog", &sv(&ctx, &s, &a));
            assert_ok("click", &click.join().unwrap());
            assert_eq!(
                eval(&ctx, &s, &page, "document.getElementById('o').textContent"),
                want,
                "{label} {answer:?}"
            );
        }
    }
}

// (s) A service worker's own responses: one it builds and one it takes
// from its cache reach the page without a network request; the proxy
// reports each as an `sw` line carrying the request and the response.
#[test]
fn cell_intercept_reports_service_worker_responses() {
    for _ in each_available_backend() {
        let ctx = TestContext::start();
        let s = intercept_session(&ctx);
        let r = sv(&ctx, &s, &["open", &purl(&ctx, "/sw.html")]);
        assert_ok("open", &r);
        let page = body_first(&r);
        let got = eventually(&ctx, &s, &page, "String(window.__sw)", "built,from-cache");
        assert_eq!(got, "built,from-cache");
        let mut reports = Vec::new();
        let start = Instant::now();
        while reports.len() < 2 && start.elapsed() < Duration::from_secs(10) {
            let r = sv(&ctx, &s, &["net", "poll", "--wait-ms", "300"]);
            assert_ok("net poll", &r);
            for l in r.stdout.lines() {
                if let Some(b) = l.strip_prefix("sw ") {
                    reports.push(String::from_utf8(B64.decode(b).unwrap()).unwrap());
                }
            }
        }
        let built = reports
            .iter()
            .find(|r| r.contains("/sw-built"))
            .unwrap_or_else(|| panic!("no report for /sw-built: {reports:?}"));
        assert!(built.contains("\"status\":201"), "{built}");
        assert!(built.contains("\"cached\":false"), "{built}");
        let cached = reports
            .iter()
            .find(|r| r.contains("/sw-cached"))
            .unwrap_or_else(|| panic!("no report for /sw-cached: {reports:?}"));
        assert!(cached.contains("\"cached\":true"), "{cached}");
    }
}

// (t) An upstream with a certificate nobody trusts: refused by default,
// fetched with `--ignore-https-errors`.
#[test]
fn cell_intercept_ignore_https_errors() {
    for _ in each_available_backend() {
        let up = HitServer::tls();
        let ctx = TestContext::start();
        let url = up.url("/hit");
        let fetch = format!(
            "(fetch('{url}').then(r => r.text()).then(t => window.__hit = 'got ' + t, e => window.__hit = 'ERR ' + e), 'started')"
        );
        for (flags, want) in [
            (vec!["--intercept"], "ERR"),
            (vec!["--intercept", "--ignore-https-errors"], "got"),
        ] {
            let mut a = vec!["session-open"];
            a.extend(flags);
            let r = ctx.vs(&a);
            assert_ok("session-open", &r);
            let s = body_first(&r);
            let r = sv(&ctx, &s, &["open", &purl(&ctx, "/net.html")]);
            assert_ok("open", &r);
            let page = body_first(&r);
            eval(&ctx, &s, &page, &fetch);
            let got = eventually(&ctx, &s, &page, "String(window.__hit)", want);
            assert!(got.starts_with(want), "{a:?}: {got}");
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

// (w) WebSockets pass through an intercept session's proxy, on a named
// host and on loopback (WebKitGTK and WebView2 proxy loopback too).
#[test]
fn cell_intercept_passes_websockets() {
    for (_, loopback) in each_available_backend().flat_map(|b| [(b, false), (b, true)]) {
        let ctx = TestContext::start();
        let s = intercept_session(&ctx);
        let r = sv(&ctx, &s, &["open", &hurl(&ctx, loopback, "/net.html")]);
        assert_ok("open", &r);
        let page = body_first(&r);
        eval(
            &ctx,
            &s,
            &page,
            "(() => { window.__ws = 'pending'; const w = new WebSocket(`ws://${location.host}/ws`); w.onopen = () => w.send('hi'); w.onmessage = (e) => { window.__ws = e.data; }; w.onerror = () => { window.__ws = 'error'; }; return 'ok'; })()",
        );
        let got = eventually(&ctx, &s, &page, "String(window.__ws)", "echo:hi");
        if !got.contains("echo:hi") {
            let r = sv(&ctx, &s, &["net", "poll", "--wait-ms", "0"]);
            let log =
                std::fs::read_to_string(ctx.home_path().join("daemon.log")).unwrap_or_default();
            panic!(
                "loopback={loopback}: {got}\nproxy events:\n{}\ndaemon log:\n{log}",
                r.stdout
            );
        }
    }
}
