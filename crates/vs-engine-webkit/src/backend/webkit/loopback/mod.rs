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

mod http;
mod response;

use http::exchange;
use response::on_msg;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, Bool, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_foundation::{
    NSArray, NSDate, NSHTTPCookie, NSObject, NSObjectProtocol, NSString, NSURLRequest,
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
