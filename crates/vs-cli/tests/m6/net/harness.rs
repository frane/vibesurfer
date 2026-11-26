//! Helpers the interception cells share: parsing `vs net poll` events,
//! fixture URLs and driving a page through the CLI.

use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;

use crate::support::{assert_ok, body_first, vs, TestContext};

/// One parsed `vs net poll` event.
#[derive(Debug, Clone)]
pub(super) struct Ev {
    /// `req`, `res`, `fin`, `fail`, `drop`.
    pub(super) kind: String,
    pub(super) id: u64,
    /// The rest of the event's first line after kind and id.
    pub(super) line: String,
    pub(super) headers: Vec<(String, String)>,
    pub(super) body: Option<Vec<u8>>,
}

impl Ev {
    pub(super) fn url(&self) -> &str {
        // req: <METHOD> <url> ...; res: <status> <url> ...
        self.line.split(' ').nth(1).unwrap_or("")
    }
    pub(super) fn field(&self, key: &str) -> Option<&str> {
        self.line
            .split(' ')
            .find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
    }
}

pub(super) fn parse_events(body: &str) -> Vec<Ev> {
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
pub(super) fn purl(ctx: &TestContext, path: &str) -> String {
    ctx.url(path).replace("//127.0.0.1:", "//vs.localhost:")
}

/// The fixture URL on 127.0.0.1 for `loopback`, else under a name.
pub(super) fn hurl(ctx: &TestContext, loopback: bool, path: &str) -> String {
    if loopback {
        ctx.url(path)
    } else {
        purl(ctx, path)
    }
}

pub(super) fn intercept_session(ctx: &TestContext) -> String {
    let r = ctx.vs(&["session-open", "--intercept"]);
    assert_ok("session-open --intercept", &r);
    body_first(&r)
}

pub(super) fn sv(ctx: &TestContext, s: &str, args: &[&str]) -> crate::support::CliOut {
    let mut a: Vec<&str> = args.to_vec();
    let flag = format!("--session={s}");
    a.push(&flag);
    ctx.vs(&a)
}

pub(super) fn poll(ctx: &TestContext, s: &str, wait_ms: u64) -> Vec<Ev> {
    let r = sv(ctx, s, &["net", "poll", "--wait-ms", &wait_ms.to_string()]);
    assert_ok("net poll", &r);
    let body: String = r.stdout.lines().skip(1).collect::<Vec<_>>().join("\n");
    parse_events(&body)
}

pub(super) fn eval(ctx: &TestContext, s: &str, page: &str, expr: &str) -> String {
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
pub(super) fn drive(
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
pub(super) fn eventually(ctx: &TestContext, s: &str, page: &str, expr: &str, want: &str) -> String {
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
pub(super) fn goto_async(
    ctx: &TestContext,
    s: &str,
    page: &str,
    url: &str,
) -> std::thread::JoinHandle<()> {
    let home = ctx.home_path().to_path_buf();
    let (s, page, url) = (s.to_string(), page.to_string(), url.to_string());
    std::thread::spawn(move || {
        let r = vs(&home, &["goto", &page, &url, &format!("--session={s}")]);
        assert_ok("goto", &r);
    })
}

pub(super) fn open_blank(ctx: &TestContext, s: &str) -> String {
    let r = sv(ctx, s, &["open", "about:blank"]);
    assert_ok("open about:blank", &r);
    body_first(&r)
}

pub(super) fn find<'a>(evs: &'a [Ev], kind: &str, url_end: &str) -> Option<&'a Ev> {
    evs.iter()
        .find(|e| e.kind == kind && e.url().ends_with(url_end))
}

/// How many requests whose URL ends with `url_end` have finished. The
/// browser's own traffic (WebView2 reaches SmartScreen through the
/// proxy too) finishes on its own schedule and must not count.
pub(super) fn fins(evs: &[Ev], url_end: &str) -> usize {
    evs.iter()
        .filter(|f| f.kind == "fin")
        .filter(|f| {
            evs.iter()
                .any(|r| r.kind == "req" && r.id == f.id && r.url().ends_with(url_end))
        })
        .count()
}

pub(super) fn status_of(evs: &[Ev], id: u64) -> Option<String> {
    evs.iter()
        .find(|e| e.kind == "res" && e.id == id)
        .map(|e| e.line.split(' ').next().unwrap_or("").to_string())
}
