//! Request interception for `--intercept` sessions.
//!
//! Each intercepting session gets its own HTTP proxy on
//! `127.0.0.1:<ephemeral port>`, and the engine points that session's
//! data store at it (see `vs_engine_webkit::DataStore::Proxied`). The
//! port is the demux: everything arriving on it belongs to the session.
//!
//! The proxy records every request and response as wire lines in a
//! queue the client drains with `vs_net_poll`. With routing on, each
//! request is held after its `req` line until the client continues,
//! fulfills or aborts it (or 30 s pass, which continues it). Response
//! bodies are kept, newest first, up to 16 MiB per session, for
//! `vs_net_body`.
//!
//! HTTPS arrives as `CONNECT host:port`; the proxy terminates the
//! browser's TLS with a leaf certificate from the daemon's CA (see
//! [`tls`]) and handles the requests inside like plain ones, with
//! `https://` URLs. Upstream TLS is verified as usual.

mod classify;
pub(crate) mod http;
mod proxy;
#[cfg(test)]
mod tests;
pub mod tls;

pub use tls::Mitm;

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;

use http::Headers;

/// A held request with no decision continues after this long, so a
/// client that died never wedges a page.
pub const HOLD_TIMEOUT: Duration = Duration::from_secs(30);
/// Response bodies kept per session, in total.
const BODY_CACHE_BYTES: usize = 16 * 1024 * 1024;
/// Undrained events kept per session; older ones are dropped and
/// counted in a `drop <n>` line.
const MAX_EVENTS: usize = 20_000;

/// A request as the proxy received it (or as overrides changed it).
#[derive(Debug, Clone)]
pub struct ProxyRequest {
    pub method: String,
    /// Absolute `http://` or `https://` URL.
    pub url: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

/// Client changes to a request before it goes out.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub url: Option<String>,
    pub method: Option<String>,
    /// Replaces the whole header list when set.
    pub headers: Option<Headers>,
    pub body: Option<Vec<u8>>,
}

impl Overrides {
    fn apply(&self, req: &ProxyRequest) -> (ProxyRequest, bool) {
        let url_changed = self.url.as_ref().is_some_and(|u| *u != req.url);
        let out = ProxyRequest {
            method: self.method.clone().unwrap_or_else(|| req.method.clone()),
            url: self.url.clone().unwrap_or_else(|| req.url.clone()),
            headers: self.headers.clone().unwrap_or_else(|| req.headers.clone()),
            body: self.body.clone().unwrap_or_else(|| req.body.clone()),
        };
        (out, url_changed)
    }
}

/// What the client decided for a held request.
#[derive(Debug, Clone)]
pub enum Decision {
    Continue(Overrides),
    Fulfill {
        status: u16,
        headers: Headers,
        body: Vec<u8>,
    },
    /// Close the browser's connection without a response. The string
    /// is the error name reported in the `fail` line.
    Abort(String),
}

/// A response read in full for `vs_net_fetch`.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub status: u16,
    pub reason: String,
    pub url: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

struct Held {
    req: ProxyRequest,
    tx: mpsc::Sender<Decision>,
}

#[derive(Default)]
struct Events {
    q: VecDeque<String>,
    dropped: u64,
}

#[derive(Default)]
struct Bodies {
    q: VecDeque<(u64, Vec<u8>)>,
    bytes: usize,
}

/// One session's proxy and its client-facing state.
pub struct SessionNet {
    port: u16,
    mitm: Arc<Mitm>,
    next_id: AtomicU64,
    route: AtomicBool,
    closed: AtomicBool,
    events: Mutex<Events>,
    events_cv: Condvar,
    held: Mutex<HashMap<u64, Held>>,
    bodies: Mutex<Bodies>,
}

impl SessionNet {
    /// Bind a proxy on an ephemeral loopback port and start serving.
    /// `mitm` answers the browser's TLS and checks upstream TLS.
    pub fn start(mitm: Arc<Mitm>) -> io::Result<Arc<Self>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let net = Arc::new(Self {
            port,
            mitm,
            next_id: AtomicU64::new(1),
            route: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            events: Mutex::new(Events::default()),
            events_cv: Condvar::new(),
            held: Mutex::new(HashMap::new()),
            bodies: Mutex::new(Bodies::default()),
        });
        let for_thread = net.clone();
        std::thread::Builder::new()
            .name(format!("vs-net-{port}"))
            .spawn(move || proxy::serve(&listener, &for_thread))?;
        Ok(net)
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stop accepting connections, release held requests and wake
    /// pollers. Connections already open end when the browser closes
    /// them.
    pub fn shutdown(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.set_route(false);
        // Wake the blocking accept so the thread sees `closed`.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        self.events_cv.notify_all();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn routing(&self) -> bool {
        self.route.load(Ordering::SeqCst)
    }

    /// Turn holding on or off. Off continues every held request.
    pub fn set_route(&self, on: bool) {
        self.route.store(on, Ordering::SeqCst);
        if !on {
            let drained: Vec<Held> = self
                .held
                .lock()
                .expect("poisoned")
                .drain()
                .map(|(_, h)| h)
                .collect();
            for h in drained {
                let _ = h.tx.send(Decision::Continue(Overrides::default()));
            }
        }
    }

    /// Hand `decision` to held request `id`. False if `id` is not held
    /// (unknown, already decided, or timed out).
    pub fn decide(&self, id: u64, decision: Decision) -> bool {
        let held = self.held.lock().expect("poisoned").remove(&id);
        held.is_some_and(|h| h.tx.send(decision).is_ok())
    }

    /// Perform held request `id` (with `o` applied) against the network
    /// and return the response without delivering it. The request stays
    /// held.
    pub fn fetch(&self, id: u64, o: &Overrides) -> Result<Fetched, FetchError> {
        let req = self
            .held
            .lock()
            .expect("poisoned")
            .get(&id)
            .map(|h| h.req.clone())
            .ok_or(FetchError::NotHeld)?;
        let (out, url_changed) = o.apply(&req);
        proxy::fetch_full(&self.mitm, &out, url_changed)
            .map(|(status, reason, headers, body)| Fetched {
                status,
                reason,
                url: out.url.clone(),
                headers,
                body,
            })
            .map_err(|e| FetchError::Network(e.to_string()))
    }

    /// A kept response body.
    pub fn body(&self, id: u64) -> Option<Vec<u8>> {
        self.bodies
            .lock()
            .expect("poisoned")
            .q
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, b)| b.clone())
    }

    /// Drain queued event lines. With `wait`, block up to that long for
    /// the first one.
    pub fn poll(&self, wait: Duration) -> String {
        let mut ev = self.events.lock().expect("poisoned");
        if ev.q.is_empty() && !wait.is_zero() && !self.is_closed() {
            ev = self
                .events_cv
                .wait_timeout_while(ev, wait, |e| e.q.is_empty() && !self.is_closed())
                .expect("poisoned")
                .0;
        }
        let mut out = String::new();
        if ev.dropped > 0 {
            let _ = writeln!(out, "drop {}", ev.dropped);
            ev.dropped = 0;
        }
        for e in ev.q.drain(..) {
            out.push_str(&e);
        }
        out
    }

    // ---------------------------------------------------------------
    // Proxy-side hooks.
    // ---------------------------------------------------------------

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    fn push(&self, text: String) {
        let mut ev = self.events.lock().expect("poisoned");
        if ev.q.len() >= MAX_EVENTS {
            ev.q.pop_front();
            ev.dropped += 1;
        }
        ev.q.push_back(text);
        drop(ev);
        self.events_cv.notify_all();
    }

    /// Park the calling connection thread until the client decides, or
    /// until `gone()` says the browser gave up on the request (the page
    /// aborted the fetch, removed the frame, navigated away): then it is
    /// dropped as `cancelled`, and a late decision gets NOT_FOUND.
    fn hold(&self, id: u64, req: &ProxyRequest, gone: &dyn Fn() -> bool) -> Decision {
        let (tx, rx) = mpsc::channel();
        self.held.lock().expect("poisoned").insert(
            id,
            Held {
                req: req.clone(),
                tx,
            },
        );
        // Routing may have been switched off between the caller's check
        // and the insert; nothing would release this one then.
        if !self.routing() {
            self.held.lock().expect("poisoned").remove(&id);
            return Decision::Continue(Overrides::default());
        }
        let deadline = std::time::Instant::now() + HOLD_TIMEOUT;
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(d) => return d,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if gone() && self.held.lock().expect("poisoned").remove(&id).is_some() {
                return Decision::Abort("cancelled".into());
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        self.held.lock().expect("poisoned").remove(&id);
        Decision::Continue(Overrides::default())
    }

    fn ev_request(&self, id: u64, req: &ProxyRequest, kind: classify::Kind) {
        let mut s = format!(
            "req {id} {} {} type={} nav={}",
            req.method,
            one_line(&req.url),
            kind.ty,
            u8::from(kind.nav)
        );
        if let Some(f) = kind.frame {
            let _ = write!(s, " frame={f}");
        }
        s.push('\n');
        push_headers(&mut s, &req.headers);
        if !req.body.is_empty() {
            let _ = writeln!(s, "body {}", B64.encode(&req.body));
        }
        self.push(s);
    }

    fn ev_response(&self, id: u64, status: u16, reason: &str, url: &str, headers: &Headers) {
        let mut s = format!("res {id} {status} {} {}\n", one_line(url), one_line(reason));
        push_headers(&mut s, headers);
        self.push(s);
    }

    fn ev_finished(&self, id: u64, size: u64) {
        self.push(format!("fin {id} size={size}\n"));
    }

    fn ev_failed(&self, id: u64, error: &str) {
        self.push(format!("fail {id} {}\n", one_line(error)));
    }

    fn keep_body(&self, id: u64, body: Vec<u8>) {
        if body.len() > BODY_CACHE_BYTES {
            return;
        }
        let mut b = self.bodies.lock().expect("poisoned");
        b.bytes += body.len();
        b.q.push_back((id, body));
        while b.bytes > BODY_CACHE_BYTES {
            match b.q.pop_front() {
                Some((_, old)) => b.bytes -= old.len(),
                None => break,
            }
        }
    }
}

impl std::fmt::Debug for SessionNet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionNet")
            .field("port", &self.port)
            .field("route", &self.routing())
            .finish_non_exhaustive()
    }
}

/// Why `vs_net_fetch` produced no response.
#[derive(Debug)]
pub enum FetchError {
    NotHeld,
    Network(String),
}

/// `res <id> <status> <url> <reason>` plus header lines; the shape
/// `vs_net_poll` and `vs_net_fetch` share.
#[must_use]
pub fn render_response(id: u64, f: &Fetched) -> String {
    let mut s = format!(
        "res {id} {} {} {}\n",
        f.status,
        one_line(&f.url),
        one_line(&f.reason)
    );
    push_headers(&mut s, &f.headers);
    let _ = writeln!(s, "body {}", B64.encode(&f.body));
    s
}

fn push_headers(s: &mut String, headers: &Headers) {
    for (k, v) in headers {
        let _ = writeln!(s, "h {}: {}", one_line(k), one_line(v));
    }
}

fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ").trim_end().to_string()
}

/// Parse `name: value` lines (as `--headers` carries them).
#[must_use]
pub fn parse_header_lines(text: &str) -> Headers {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_string(), v.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn overrides_replace_only_what_is_set() {
        let req = ProxyRequest {
            method: "GET".into(),
            url: "http://a/x".into(),
            headers: vec![("A".into(), "1".into())],
            body: vec![],
        };
        let (out, changed) = Overrides::default().apply(&req);
        assert!(!changed);
        assert_eq!(out.headers, req.headers);
        let o = Overrides {
            url: Some("http://b/y".into()),
            headers: Some(vec![]),
            ..Overrides::default()
        };
        let (out, changed) = o.apply(&req);
        assert!(changed);
        assert_eq!(
            (out.method.as_str(), out.url.as_str()),
            ("GET", "http://b/y")
        );
        assert!(out.headers.is_empty());
    }

    #[test]
    fn body_cache_evicts_oldest() {
        let net = SessionNet::start(Arc::new(Mitm::new(None, vec![]).unwrap())).unwrap();
        net.keep_body(1, vec![0; BODY_CACHE_BYTES / 2]);
        net.keep_body(2, vec![0; BODY_CACHE_BYTES / 2]);
        net.keep_body(3, vec![1; 10]);
        assert!(net.body(1).is_none());
        assert_eq!(net.body(3).unwrap(), vec![1; 10]);
        net.shutdown();
    }

    #[test]
    fn header_lines_parse() {
        assert_eq!(
            parse_header_lines("X-A: 1\nbad\nX-B:two: parts\n"),
            vec![
                ("X-A".to_string(), "1".to_string()),
                ("X-B".to_string(), "two: parts".to_string())
            ]
        );
    }
}
