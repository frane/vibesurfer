//! Loopback requests of intercept pages, through the session's proxy.
//!
//! WebKit on macOS never sends `localhost`, `127.0.0.1` or `::1`
//! through a proxy, whatever `proxyConfigurations` says (see
//! `proxy.rs`). A page served from loopback would then load around the
//! intercept proxy: no request events, no routes for its images,
//! scripts and stylesheets.
//!
//! For those pages the web view gets a `WKURLSchemeHandler` for `http`
//! and `https` instead. WebKit refuses one for its own schemes, so
//! `+[WKWebView handlesURLScheme:]` answers NO for them (once per
//! process). Every request of such a page then arrives here, goes to
//! the proxy as an absolute-form request on a worker thread, and the
//! answer is fed back to WebKit on the main queue. The proxy treats it
//! like any other request: events, routes and bodies are the same.
//!
//! A scheme handler sits above WebKit's network process, so three
//! things it would do itself are done here:
//!
//! - **Cookies.** The request leaves with a `Cookie` header built from
//!   the data store's jar, and every `Set-Cookie` of the response goes
//!   into the jar before WebKit sees the response, so `document.cookie`
//!   and later requests agree with it.
//! - **Redirects.** A 3xx with a `Location` is reported through
//!   `_willPerformRedirection:newRequest:completionHandler:` and the
//!   next hop goes out on the same task, so the page's URL and
//!   `response.redirected` come out as with the network.
//! - **Framing.** Chunked and length-delimited bodies are decoded
//!   before they reach WebKit.
//!
//! Pages loaded this way are not controlled by service workers, which
//! intercept in the network process. Only loopback pages pay that.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, Bool, ProtocolObject, Sel};
use objc2::{
    define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
    Message,
};
use objc2_foundation::{
    NSArray, NSData, NSDate, NSDictionary, NSError, NSHTTPCookie, NSHTTPURLResponse,
    NSMutableURLRequest, NSObject, NSObjectProtocol, NSString, NSURLRequest, NSURL,
};
use objc2_web_kit::{
    WKHTTPCookieStore, WKURLSchemeHandler, WKURLSchemeTask, WKWebView, WKWebViewConfiguration,
};

use crate::backend::native_download::SharedDownloads;

/// Give up on a hop whose proxy connection says nothing for this long.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Whether `url` is served from a loopback address, which WebKit on
/// macOS sends around any proxy.
pub(super) fn is_loopback(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    let host = host.to_ascii_lowercase();
    let host = host.trim_end_matches('.');
    host == "localhost"
        || host == "::1"
        || host == "0.0.0.0"
        || host.strip_prefix("127.").is_some_and(|r| {
            r.split('.').count() == 3 && r.split('.').all(|o| o.parse::<u8>().is_ok())
        })
}

/// Install the handler for `http` and `https` on `config`; requests go
/// to the proxy on `127.0.0.1:port`.
pub(super) fn install(mtm: MainThreadMarker, config: &WKWebViewConfiguration, port: u16) {
    allow_http_handlers();
    let handler = LoopbackHandler::new(mtm, port);
    let proto: &ProtocolObject<dyn WKURLSchemeHandler> = ProtocolObject::from_ref(&*handler);
    for scheme in ["http", "https"] {
        let s = NSString::from_str(scheme);
        let r = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
            config.setURLSchemeHandler_forURLScheme(Some(proto), &s);
        }));
        if r.is_err() {
            eprintln!("[vs] loopback: WebKit refused a handler for {scheme}");
        }
    }
}

static SWIZZLE: Once = Once::new();

/// `+[WKWebView handlesURLScheme:]` answers NO for `http` and `https`,
/// which is what `setURLSchemeHandler:forURLScheme:` checks before it
/// takes a handler for a scheme.
fn allow_http_handlers() {
    SWIZZLE.call_once(|| {
        type Orig = unsafe extern "C-unwind" fn(&AnyClass, Sel, &NSString) -> Bool;
        static ORIG: std::sync::OnceLock<Orig> = std::sync::OnceLock::new();
        unsafe extern "C-unwind" fn handles(cls: &AnyClass, cmd: Sel, scheme: &NSString) -> Bool {
            let s = scheme.to_string().to_ascii_lowercase();
            if s == "http" || s == "https" {
                return Bool::NO;
            }
            match ORIG.get() {
                Some(f) => unsafe { f(cls, cmd, scheme) },
                None => Bool::NO,
            }
        }
        unsafe {
            let meta = AnyClass::get(c"WKWebView").map(|c| c.metaclass());
            let Some(meta) = meta else { return };
            let Some(method) = meta.instance_method(sel!(handlesURLScheme:)) else {
                return;
            };
            let new: Orig = handles;
            let prev =
                method.set_implementation(std::mem::transmute::<Orig, objc2::runtime::Imp>(new));
            let _ = ORIG.set(std::mem::transmute::<objc2::runtime::Imp, Orig>(prev));
        }
    });
}

pub(super) struct HandlerIvars {
    port: u16,
}

define_class!(
    /// `WKURLSchemeHandler` for `http`/`https` on loopback pages.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "VsLoopbackHandler"]
    #[ivars = HandlerIvars]
    pub(super) struct LoopbackHandler;

    unsafe impl NSObjectProtocol for LoopbackHandler {}

    unsafe impl WKURLSchemeHandler for LoopbackHandler {
        #[unsafe(method(webView:startURLSchemeTask:))]
        fn start(&self, web_view: &WKWebView, task: &ProtocolObject<dyn WKURLSchemeTask>) {
            start_task(self.ivars().port, web_view, task);
        }

        #[unsafe(method(webView:stopURLSchemeTask:))]
        fn stop(&self, _web_view: &WKWebView, task: &ProtocolObject<dyn WKURLSchemeTask>) {
            stop_task(task);
        }
    }
);

impl LoopbackHandler {
    fn new(mtm: MainThreadMarker, port: u16) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(HandlerIvars { port });
        unsafe { msg_send![super(this), init] }
    }
}

/// One request leg sent to the proxy.
#[derive(Clone)]
struct Hop {
    method: String,
    url: String,
    /// Request headers as WebKit set them, without `Cookie`.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct TaskState {
    task: Retained<ProtocolObject<dyn WKURLSchemeTask>>,
    cookies: Retained<WKHTTPCookieStore>,
    port: u16,
    hop: Hop,
    /// Counts hops, so a superseded hop's late messages are dropped.
    generation: u64,
    redirects: u32,
    stopped: Arc<AtomicBool>,
    /// WebKit has the response; data can go straight through.
    responded: bool,
    queued: Vec<Vec<u8>>,
    finished: bool,
    /// A copy of a document's body, in case it turns into a download
    /// (see [`capture`]).
    tee: Option<Vec<u8>>,
    /// WebKit let go of the task for a download: the body goes to
    /// `download`, not to WebKit.
    download: Option<(SharedDownloads, u64)>,
}

thread_local! {
    /// Downloads the navigation delegate asked for that their task has
    /// not handed over yet: response URL to buffer and entry.
    static CAPTURES: RefCell<HashMap<String, (SharedDownloads, u64)>> = RefCell::new(HashMap::new());
    /// Bodies of documents that finished, newest last.
    static RECENT: RefCell<Vec<(String, Vec<u8>)>> = const { RefCell::new(Vec::new()) };
}

/// A response on a loopback page that is a download: WebKit cannot turn
/// a scheme-handler load into a `WKDownload`, so the navigation
/// delegate cancels it instead and the handler, which has the bytes,
/// finishes download `id` in `downloads` with them.
pub(super) fn capture(url: &str, downloads: SharedDownloads, id: u64) {
    let done = RECENT.with(|r| {
        let mut r = r.borrow_mut();
        let i = r.iter().position(|(u, _)| u == url)?;
        Some(r.remove(i).1)
    });
    match done {
        Some(bytes) => {
            if let Ok(mut d) = downloads.lock() {
                d.finish(id, Ok(bytes));
            }
        }
        None => {
            CAPTURES.with(|c| c.borrow_mut().insert(url.to_string(), (downloads, id)));
        }
    }
}

/// The web view sends its http(s) through this handler.
pub(super) fn handles(web_view: &WKWebView) -> bool {
    unsafe {
        web_view
            .configuration()
            .urlSchemeHandlerForURLScheme(&NSString::from_str("http"))
            .is_some()
    }
}

thread_local! {
    static TASKS: RefCell<HashMap<u64, TaskState>> = RefCell::new(HashMap::new());
    /// Task object address to id, for `stop`.
    static BY_ADDR: RefCell<HashMap<usize, u64>> = RefCell::new(HashMap::new());
    static NEXT: Cell<u64> = const { Cell::new(1) };
}

fn addr(task: &ProtocolObject<dyn WKURLSchemeTask>) -> usize {
    std::ptr::from_ref(task).cast::<c_void>() as usize
}

fn start_task(port: u16, web_view: &WKWebView, task: &ProtocolObject<dyn WKURLSchemeTask>) {
    let req = unsafe { task.request() };
    let url = req
        .URL()
        .and_then(|u| u.absoluteString())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let method = req
        .HTTPMethod()
        .map_or_else(|| "GET".to_string(), |m| m.to_string());
    let mut headers = Vec::new();
    if let Some(dict) = req.allHTTPHeaderFields() {
        let (keys, values) = dict.to_vecs();
        for (k, v) in keys.into_iter().zip(values) {
            if !k.to_string().eq_ignore_ascii_case("cookie") {
                headers.push((k.to_string(), v.to_string()));
            }
        }
    }
    let body = request_body(&req);
    let cookies = unsafe {
        web_view
            .configuration()
            .websiteDataStore()
            .httpCookieStore()
    };
    let id = NEXT.with(|n| {
        let v = n.get();
        n.set(v + 1);
        v
    });
    TASKS.with(|t| {
        t.borrow_mut().insert(
            id,
            TaskState {
                task: task.retain(),
                cookies,
                port,
                hop: Hop {
                    method,
                    url,
                    headers,
                    body,
                },
                generation: 0,
                redirects: 0,
                stopped: Arc::new(AtomicBool::new(false)),
                responded: false,
                queued: Vec::new(),
                finished: false,
                tee: None,
                download: None,
            },
        );
    });
    BY_ADDR.with(|b| b.borrow_mut().insert(addr(task), id));
    send_hop(id);
}

fn stop_task(task: &ProtocolObject<dyn WKURLSchemeTask>) {
    let Some(id) = BY_ADDR.with(|b| b.borrow_mut().remove(&addr(task))) else {
        return;
    };
    // A download the delegate cancelled the navigation for: keep
    // reading, into the download.
    let kept = TASKS.with(|t| {
        let mut t = t.borrow_mut();
        let s = t.get_mut(&id)?;
        let target = CAPTURES.with(|c| c.borrow_mut().remove(&s.hop.url))?;
        s.download = Some(target);
        if s.finished {
            Some(true)
        } else {
            Some(false)
        }
    });
    match kept {
        Some(true) => finish_download(id, Ok(())),
        Some(false) => {}
        None => {
            if let Some(s) = TASKS.with(|t| t.borrow_mut().remove(&id)) {
                s.stopped.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Hand a captured download its bytes (or its failure) and drop the task.
fn finish_download(id: u64, result: Result<(), String>) {
    let Some(s) = TASKS.with(|t| t.borrow_mut().remove(&id)) else {
        return;
    };
    if let Some((downloads, dl)) = s.download {
        let bytes = result.map(|()| s.tee.unwrap_or_default());
        if let Ok(mut d) = downloads.lock() {
            d.finish(dl, bytes);
        }
    }
}

fn forget(id: u64) {
    if let Some(s) = TASKS.with(|t| t.borrow_mut().remove(&id)) {
        BY_ADDR.with(|b| b.borrow_mut().remove(&addr(&s.task)));
        // A document that finished before the delegate decided it is a
        // download: keep its body for `capture`.
        if let (Some(tee), true) = (s.tee, s.finished) {
            RECENT.with(|r| {
                let mut r = r.borrow_mut();
                r.retain(|(u, _)| *u != s.hop.url);
                r.push((s.hop.url.clone(), tee));
                if r.len() > 8 {
                    r.remove(0);
                }
            });
        }
    }
}

/// The request body: `HTTPBody`, or the stream WebKit hands over for
/// blobs and form data, read to its end.
fn request_body(req: &NSURLRequest) -> Vec<u8> {
    if let Some(d) = req.HTTPBody() {
        return d.to_vec();
    }
    let Some(stream) = req.HTTPBodyStream() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    unsafe {
        let _: () = msg_send![&*stream, open];
        loop {
            let n: isize = msg_send![&*stream, read: buf.as_mut_ptr(), maxLength: buf.len()];
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n.unsigned_abs()]);
        }
        let _: () = msg_send![&*stream, close];
    }
    out
}

/// Read the jar, then send the task's current hop to the proxy.
fn send_hop(id: u64) {
    let Some((cookies, generation)) = TASKS.with(|t| {
        t.borrow()
            .get(&id)
            .map(|s| (s.cookies.clone(), s.generation))
    }) else {
        return;
    };
    let block = RcBlock::new(move |list: NonNull<NSArray<NSHTTPCookie>>| {
        let list: &NSArray<NSHTTPCookie> = unsafe { list.as_ref() };
        let Some((hop, port, stopped)) = TASKS.with(|t| {
            t.borrow()
                .get(&id)
                .filter(|s| s.generation == generation)
                .map(|s| (s.hop.clone(), s.port, s.stopped.clone()))
        }) else {
            return;
        };
        let cookie = cookie_header(list, &hop.url);
        std::thread::spawn(move || {
            exchange(id, generation, port, &hop, cookie.as_deref(), &stopped);
        });
    });
    unsafe { cookies.getAllCookies(&block) };
}

/// `name=value; …` of the jar's cookies that go with a request to
/// `url` (RFC 6265 domain, path, secure and expiry rules), longest
/// path first.
fn cookie_header(list: &NSArray<NSHTTPCookie>, url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let path = &rest[authority.len()..];
    let path = path
        .split(['?', '#'])
        .next()
        .filter(|p| !p.is_empty())
        .unwrap_or("/");
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("").to_string()
    } else {
        authority
            .split(':')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    let now = NSDate::now();
    let mut picked: Vec<(usize, String)> = Vec::new();
    for i in 0..list.count() {
        let c = list.objectAtIndex(i);
        let domain = c.domain().to_string().to_ascii_lowercase();
        let domain_ok = match domain.strip_prefix('.') {
            Some(d) => host == d || host.ends_with(&format!(".{d}")),
            None => host == domain,
        };
        if !domain_ok {
            continue;
        }
        let cpath = c.path().to_string();
        let path_ok = path == cpath
            || (path.starts_with(&cpath)
                && (cpath.ends_with('/') || path[cpath.len()..].starts_with('/')));
        if !path_ok {
            continue;
        }
        if c.isSecure() && scheme != "https" && !is_loopback(url) {
            continue;
        }
        if let Some(exp) = c.expiresDate() {
            if exp.timeIntervalSinceDate(&now) <= 0.0 {
                continue;
            }
        }
        picked.push((cpath.len(), format!("{}={}", c.name(), c.value())));
    }
    if picked.is_empty() {
        return None;
    }
    picked.sort_by_key(|p| std::cmp::Reverse(p.0));
    Some(
        picked
            .into_iter()
            .map(|(_, s)| s)
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// What a worker reports back for a hop.
enum Msg {
    Head {
        status: u16,
        headers: Vec<(String, String)>,
    },
    Data(Vec<u8>),
    Done,
    Fail(String),
}

/// Run `f(id, generation, msg)` on the main queue.
fn post(id: u64, generation: u64, msg: Msg) {
    struct Job {
        id: u64,
        generation: u64,
        msg: Msg,
    }
    unsafe extern "C" fn run(ctx: *mut c_void) {
        let job = unsafe { Box::from_raw(ctx.cast::<Job>()) };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            on_msg(job.id, job.generation, job.msg);
        }));
    }
    #[allow(non_upper_case_globals)]
    unsafe extern "C" {
        static _dispatch_main_q: c_void;
        fn dispatch_async_f(
            queue: *const c_void,
            ctx: *mut c_void,
            work: unsafe extern "C" fn(*mut c_void),
        );
    }
    let job = Box::into_raw(Box::new(Job {
        id,
        generation,
        msg,
    }));
    unsafe { dispatch_async_f(&raw const _dispatch_main_q, job.cast(), run) };
}

/// Worker thread: one hop through the proxy, streamed back.
fn exchange(
    id: u64,
    generation: u64,
    port: u16,
    hop: &Hop,
    cookie: Option<&str>,
    stopped: &AtomicBool,
) {
    if let Err(e) = exchange_inner(id, generation, port, hop, cookie, stopped) {
        if !stopped.load(Ordering::Relaxed) {
            post(id, generation, Msg::Fail(e.to_string()));
        }
    }
}

fn exchange_inner(
    id: u64,
    generation: u64,
    port: u16,
    hop: &Hop,
    cookie: Option<&str>,
    stopped: &AtomicBool,
) -> std::io::Result<()> {
    let mut sock = TcpStream::connect(("127.0.0.1", port))?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(READ_TIMEOUT))?;
    sock.write_all(request_head(hop, cookie).as_bytes())?;
    sock.write_all(&hop.body)?;
    sock.flush()?;

    let mut r = BufReader::new(sock);
    let (status, headers) = loop {
        let (status, headers) = read_head(&mut r)?;
        // Interim responses are not the answer.
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        break (status, headers);
    };
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
    };
    let chunked =
        header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let length = header("content-length").and_then(|v| v.parse::<u64>().ok());
    let bodyless = hop.method.eq_ignore_ascii_case("HEAD") || status == 204 || status == 304;
    post(id, generation, Msg::Head { status, headers });
    if !bodyless {
        let send = |chunk: &[u8]| {
            if !chunk.is_empty() {
                post(id, generation, Msg::Data(chunk.to_vec()));
            }
        };
        if chunked {
            read_chunked(&mut r, stopped, send)?;
        } else {
            read_plain(&mut r, length, stopped, send)?;
        }
    }
    if !stopped.load(Ordering::Relaxed) {
        post(id, generation, Msg::Done);
    }
    Ok(())
}

/// The absolute-form request line and headers for `hop`.
fn request_head(hop: &Hop, cookie: Option<&str>) -> String {
    use std::fmt::Write as _;
    let authority = hop
        .url
        .split_once("://")
        .map_or("", |(_, r)| r.split(['/', '?', '#']).next().unwrap_or(""));
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {authority}\r\n",
        hop.method, hop.url
    );
    for (k, v) in &hop.headers {
        let lk = k.to_ascii_lowercase();
        if matches!(
            lk.as_str(),
            "host" | "content-length" | "connection" | "transfer-encoding"
        ) {
            continue;
        }
        let _ = write!(head, "{k}: {v}\r\n");
    }
    if let Some(c) = cookie {
        let _ = write!(head, "Cookie: {c}\r\n");
    }
    if !hop.body.is_empty() || !matches!(hop.method.as_str(), "GET" | "HEAD" | "OPTIONS") {
        let _ = write!(head, "Content-Length: {}\r\n", hop.body.len());
    }
    // Tells the proxy to abort with a plain close (see its LOOPBACK_MARK).
    head.push_str("X-Vs-Loopback: 1\r\nConnection: close\r\n\r\n");
    head
}

/// A chunked body, decoded, to `send`.
fn read_chunked(
    r: &mut impl BufRead,
    stopped: &AtomicBool,
    send: impl Fn(&[u8]),
) -> std::io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or("0"), 16)
            .map_err(|_| std::io::Error::other("bad chunk size"))?;
        if size == 0 {
            // Trailers, up to the empty line.
            loop {
                let mut t = String::new();
                if r.read_line(&mut t)? == 0 || t.trim().is_empty() {
                    return Ok(());
                }
            }
        }
        let mut left = size;
        while left > 0 {
            if stopped.load(Ordering::Relaxed) {
                return Ok(());
            }
            let cap = left.min(buf.len());
            let n = r.read(&mut buf[..cap])?;
            if n == 0 {
                return Err(std::io::Error::other("connection closed inside a chunk"));
            }
            send(&buf[..n]);
            left -= n;
        }
        let mut crlf = String::new();
        r.read_line(&mut crlf)?;
    }
}

/// A body of `length` bytes, or up to the close without one, to `send`.
fn read_plain(
    r: &mut impl Read,
    length: Option<u64>,
    stopped: &AtomicBool,
    send: impl Fn(&[u8]),
) -> std::io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut left = length;
    loop {
        if stopped.load(Ordering::Relaxed) {
            return Ok(());
        }
        let want = left.map_or(buf.len(), |l| {
            usize::try_from(l).unwrap_or(usize::MAX).min(buf.len())
        });
        if want == 0 {
            return Ok(());
        }
        let n = r.read(&mut buf[..want])?;
        if n == 0 {
            if left.is_some_and(|l| l > 0) {
                return Err(std::io::Error::other(
                    "connection closed before the body ended",
                ));
            }
            return Ok(());
        }
        send(&buf[..n]);
        if let Some(l) = left.as_mut() {
            *l -= n as u64;
        }
    }
}

fn read_head(r: &mut impl BufRead) -> std::io::Result<(u16, Vec<(String, String)>)> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Err(std::io::Error::other(
            "the proxy closed the connection without a response",
        ));
    }
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other(format!("bad status line: {}", line.trim())))?;
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 {
            break;
        }
        let h = h.trim_end_matches(['\r', '\n']);
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok((status, headers))
}

/// Main queue: apply one worker message to its task.
fn on_msg(id: u64, generation: u64, msg: Msg) {
    let live = TASKS.with(|t| {
        t.borrow()
            .get(&id)
            .is_some_and(|s| s.generation == generation && !s.stopped.load(Ordering::Relaxed))
    });
    if !live {
        return;
    }
    match msg {
        Msg::Head { status, headers } => on_head(id, generation, status, headers),
        Msg::Data(d) => {
            let deliver = TASKS.with(|t| {
                let mut t = t.borrow_mut();
                let s = t.get_mut(&id)?;
                if let Some(tee) = s.tee.as_mut() {
                    tee.extend_from_slice(&d);
                }
                if s.download.is_some() {
                    return None;
                }
                if s.responded {
                    Some(s.task.clone())
                } else {
                    s.queued.push(d.clone());
                    None
                }
            });
            if let Some(task) = deliver {
                let data = NSData::with_bytes(&d);
                guard(|| unsafe { task.didReceiveData(&data) });
            }
        }
        Msg::Done => {
            if TASKS.with(|t| t.borrow().get(&id).is_some_and(|s| s.download.is_some())) {
                finish_download(id, Ok(()));
                return;
            }
            let finish = TASKS.with(|t| {
                let mut t = t.borrow_mut();
                let s = t.get_mut(&id)?;
                s.finished = true;
                s.responded.then(|| s.task.clone())
            });
            if let Some(task) = finish {
                guard(|| unsafe { task.didFinish() });
                forget(id);
            }
        }
        Msg::Fail(why) => {
            if TASKS.with(|t| t.borrow().get(&id).is_some_and(|s| s.download.is_some())) {
                finish_download(id, Err(why));
                return;
            }
            let task = TASKS.with(|t| t.borrow().get(&id).map(|s| s.task.clone()));
            if let Some(task) = task {
                let err = url_error(&why);
                guard(|| unsafe { task.didFailWithError(&err) });
            }
            forget(id);
        }
    }
}

/// Call into a task; a task WebKit already stopped raises, which must
/// not take the daemon down.
fn guard(f: impl FnOnce()) {
    let _ = objc2::exception::catch(std::panic::AssertUnwindSafe(f));
}

fn url_error(_why: &str) -> Retained<NSError> {
    let domain = NSString::from_str("NSURLErrorDomain");
    // NSURLErrorCannotConnectToHost
    unsafe { NSError::errorWithDomain_code_userInfo(&domain, -1004, None) }
}

fn on_head(id: u64, generation: u64, status: u16, headers: Vec<(String, String)>) {
    // A document may turn out to be a download (the delegate decides
    // once WebKit has the response): keep a copy of its body.
    TASKS.with(|t| {
        if let Some(s) = t.borrow_mut().get_mut(&id) {
            let dest = s
                .hop
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("sec-fetch-dest"))
                .map(|(_, v)| v.to_ascii_lowercase());
            let attachment = headers.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("content-disposition")
                    && crate::backend::native_download::is_attachment(v)
            });
            if attachment || matches!(dest.as_deref(), Some("document" | "iframe" | "frame")) {
                s.tee = Some(Vec::new());
            }
        }
    });
    let Some((url, cookies)) = TASKS.with(|t| {
        t.borrow()
            .get(&id)
            .map(|s| (s.hop.url.clone(), s.cookies.clone()))
    }) else {
        return;
    };
    let Some(ns_url) = NSURL::URLWithString(&NSString::from_str(&url)) else {
        return;
    };
    // Every Set-Cookie on its own, so a comma in one (an Expires date)
    // cannot split it.
    let mut jar: Vec<Retained<NSHTTPCookie>> = Vec::new();
    for (_, v) in headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
    {
        let k = NSString::from_str("Set-Cookie");
        let v = NSString::from_str(v);
        let dict = NSDictionary::<NSString, NSString>::from_retained_objects(&[&*k], &[v]);
        jar.extend(NSHTTPCookie::cookiesWithResponseHeaderFields_forURL(
            &dict, &ns_url,
        ));
    }
    let location = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("location"))
        .map(|(_, v)| v.clone());
    let response = http_response(&ns_url, status, &headers);
    let next = move || {
        let redirect = matches!(status, 301 | 302 | 303 | 307 | 308);
        match (
            redirect,
            location.as_deref().and_then(|l| resolve(&ns_url, l)),
        ) {
            (true, Some(to)) => redirect_to(id, generation, status, &response, &to),
            _ => respond(id, &response),
        }
    };
    if jar.is_empty() {
        next();
        return;
    }
    let refs: Vec<&NSHTTPCookie> = jar.iter().map(|c| &**c).collect();
    let array = NSArray::from_slice(&refs);
    let next = RefCell::new(Some(next));
    let block = RcBlock::new(move || {
        if let Some(f) = next.borrow_mut().take() {
            f();
        }
    });
    unsafe { cookies.setCookies_completionHandler(&array, Some(&block)) };
}

/// `location` resolved against `base`, as an absolute URL.
fn resolve(base: &NSURL, location: &str) -> Option<String> {
    let u = NSURL::URLWithString_relativeToURL(&NSString::from_str(location), Some(base))?;
    u.absoluteURL()
        .and_then(|a| a.absoluteString())
        .map(|s| s.to_string())
}

fn http_response(
    url: &NSURL,
    status: u16,
    headers: &[(String, String)],
) -> Retained<NSHTTPURLResponse> {
    // WebKit folds repeated fields into one, comma-joined.
    let mut merged: Vec<(String, String)> = Vec::new();
    for (k, v) in headers {
        if let Some(m) = merged.iter_mut().find(|(mk, _)| mk.eq_ignore_ascii_case(k)) {
            m.1.push_str(", ");
            m.1.push_str(v);
        } else {
            merged.push((k.clone(), v.clone()));
        }
    }
    let keys: Vec<Retained<NSString>> = merged.iter().map(|(k, _)| NSString::from_str(k)).collect();
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let values: Vec<Retained<NSString>> =
        merged.iter().map(|(_, v)| NSString::from_str(v)).collect();
    let dict = NSDictionary::<NSString, NSString>::from_retained_objects(&key_refs, &values);
    let version = NSString::from_str("HTTP/1.1");
    NSHTTPURLResponse::initWithURL_statusCode_HTTPVersion_headerFields(
        NSHTTPURLResponse::alloc(),
        url,
        isize::try_from(status).unwrap_or(200),
        Some(&version),
        Some(&dict),
    )
    .expect("NSHTTPURLResponse")
}

/// Hand WebKit the response, then whatever body already came.
fn respond(id: u64, response: &NSHTTPURLResponse) {
    let Some(task) = TASKS.with(|t| t.borrow().get(&id).map(|s| s.task.clone())) else {
        return;
    };
    guard(|| unsafe { task.didReceiveResponse(response) });
    let (queued, finished) = TASKS.with(|t| {
        let mut t = t.borrow_mut();
        let Some(s) = t.get_mut(&id) else {
            return (Vec::new(), false);
        };
        s.responded = true;
        (std::mem::take(&mut s.queued), s.finished)
    });
    for d in queued {
        let data = NSData::with_bytes(&d);
        guard(|| unsafe { task.didReceiveData(&data) });
    }
    if finished {
        guard(|| unsafe { task.didFinish() });
        forget(id);
    }
}

/// Report a redirect and send the next hop on the same task. WebKit
/// answers with the request it will follow, or nil when the page does
/// not follow (`fetch(…, { redirect: 'manual' })`), which makes the
/// redirect itself the response.
fn redirect_to(id: u64, generation: u64, status: u16, response: &NSHTTPURLResponse, to: &str) {
    let Some((task, hop, redirects)) = TASKS.with(|t| {
        t.borrow()
            .get(&id)
            .map(|s| (s.task.clone(), s.hop.clone(), s.redirects))
    }) else {
        return;
    };
    if redirects >= 20 {
        on_msg(id, generation, Msg::Fail("too many redirects".into()));
        return;
    }
    // 303, and 301/302 after a POST, turn into a GET without a body.
    let to_get =
        status == 303 || (matches!(status, 301 | 302) && hop.method.eq_ignore_ascii_case("POST"));
    let mut next = Hop {
        method: if to_get {
            "GET".into()
        } else {
            hop.method.clone()
        },
        url: to.to_string(),
        headers: hop.headers.clone(),
        body: if to_get { Vec::new() } else { hop.body.clone() },
    };
    if to_get {
        next.headers.retain(|(k, _)| {
            let k = k.to_ascii_lowercase();
            !k.starts_with("content-")
        });
    }
    let Some(ns_to) = NSURL::URLWithString(&NSString::from_str(to)) else {
        return;
    };
    let req = NSMutableURLRequest::requestWithURL(&ns_to);
    req.setHTTPMethod(&NSString::from_str(&next.method));
    for (k, v) in &next.headers {
        req.setValue_forHTTPHeaderField(Some(&NSString::from_str(v)), &NSString::from_str(k));
    }
    let response_for_manual = response.retain();
    let next = RefCell::new(Some(next));
    let block = RcBlock::new(move |chosen: *mut NSURLRequest| {
        let Some(mut hop) = next.borrow_mut().take() else {
            return;
        };
        if chosen.is_null() {
            respond(id, &response_for_manual);
            on_msg(id, generation, Msg::Done);
            return;
        }
        let chosen: &NSURLRequest = unsafe { &*chosen };
        if let Some(u) = chosen.URL().and_then(|u| u.absoluteString()) {
            hop.url = u.to_string();
        }
        let advanced = TASKS.with(|t| {
            let mut t = t.borrow_mut();
            let Some(s) = t.get_mut(&id) else {
                return false;
            };
            if s.generation != generation {
                return false;
            }
            s.generation += 1;
            s.redirects += 1;
            s.hop = hop.clone();
            s.queued.clear();
            s.finished = false;
            true
        });
        if advanced {
            send_hop(id);
        }
    });
    let r = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
        let _: () = msg_send![
            &*task,
            _willPerformRedirection: response,
            newRequest: &*req,
            completionHandler: &*block
        ];
    }));
    if r.is_err() {
        // A stopped task; nothing more to do.
        forget(id);
    }
}
