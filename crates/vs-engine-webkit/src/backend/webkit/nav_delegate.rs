//! `VsNavDelegate` — a custom main-thread-only Obj-C class that
//! implements `WKNavigationDelegate`. Drops the navigation result into
//! a shared slot when the page finishes loading or fails.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_foundation::{
    NSData, NSError, NSHTTPURLResponse, NSInteger, NSObject, NSObjectProtocol, NSRect, NSString,
    NSURLAuthenticationChallenge, NSURLCredential, NSURLRequest, NSURLResponse, NSURL,
};
use objc2_web_kit::{
    WKDownload, WKDownloadDelegate, WKNavigation, WKNavigationAction, WKNavigationDelegate,
    WKNavigationResponse, WKNavigationResponsePolicy, WKUIDelegate, WKWebView,
    WKWebViewConfiguration, WKWindowFeatures,
};

use crate::backend::native_download::{
    is_attachment, name_from_disposition, temp_path, SharedDownloads, MAX_BYTES,
};

/// Slot shared between the delegate and the Rust caller. Both ends
/// live on the main thread, so `Rc<RefCell<...>>` is fine.
///
/// `started` / `committed` are recorded separately from `done` because
/// the web-process launch and the page load are different waits. A
/// cold `WKWebView` spends most of its first navigation spawning the
/// web-content process; folding that into the 15s load budget makes
/// `open` return `TIMEOUT` for a page that has not even started.
#[derive(Debug, Default)]
pub(super) struct NavState {
    pub started: bool,
    pub committed: bool,
    pub done: Option<Result<(), String>>,
}

impl NavState {
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }
}

pub(super) type NavSlot = Rc<RefCell<NavState>>;

pub(super) struct NavDelegateIvars {
    pub(super) slot: NavSlot,
    /// A new-window navigation WebKit asked for and we have not
    /// loaded yet. See `create_web_view`.
    pub(super) new_window: RefCell<Option<Retained<NSURLRequest>>>,
    /// Downloads the engine received, drained by `vs_download`.
    pub(super) downloads: SharedDownloads,
    /// In-flight `WKDownload`s: object address, buffer id, temp file.
    pub(super) in_flight: RefCell<Vec<(usize, u64, PathBuf)>>,
    /// The main-frame response just became a download. WebKit then
    /// fails the provisional navigation, which is not a failed load.
    pub(super) main_became_download: Cell<bool>,
    /// HTTP response of the latest main-frame navigation. Cleared at
    /// provisional start, set in `decide_response`.
    pub(super) main_response: RefCell<Option<crate::inspector::MainResponse>>,
    /// The intercept proxy's CA (DER), for intercept pages only.
    pub(super) trust_anchor: RefCell<Option<Vec<u8>>>,
}

fn ns_str(s: Option<Retained<NSString>>) -> String {
    s.map(|s| s.to_string()).unwrap_or_default()
}

fn response_url(r: &NSURLResponse) -> String {
    r.URL()
        .and_then(|u| u.absoluteString())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn disposition(r: &NSURLResponse) -> String {
    let r: Retained<NSURLResponse> = r.retain();
    r.downcast::<NSHTTPURLResponse>()
        .ok()
        .and_then(|h| h.valueForHTTPHeaderField(&NSString::from_str("Content-Disposition")))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Status, headers and URL of `r` if it is an HTTP response. WebKit
/// folds repeated headers into one comma-joined value and does not
/// expose the server's reason phrase, so the standard one is used.
fn http_response(r: &NSURLResponse) -> Option<crate::inspector::MainResponse> {
    let url = response_url(r);
    let h = r.retain().downcast::<NSHTTPURLResponse>().ok()?;
    let status = u16::try_from(h.statusCode()).unwrap_or(0);
    let (keys, values) = h.allHeaderFields().to_vecs();
    let mut headers: Vec<(String, String)> = keys
        .into_iter()
        .zip(values)
        .filter_map(|(k, v)| {
            let k = k.downcast::<NSString>().ok()?.to_string();
            let v = v.downcast::<NSString>().ok()?.to_string();
            Some((k.to_ascii_lowercase(), v))
        })
        .collect();
    headers.sort();
    Some(crate::inspector::MainResponse {
        url,
        status,
        status_text: crate::inspector::reason_phrase(status).to_string(),
        headers,
    })
}

/// Whether a navigation response should be saved instead of shown:
/// the server marked it an attachment, or WebKit cannot render it.
/// `blob:` and `data:` saves come from page script and the shim
/// already has them.
fn wants_download(resp: &WKNavigationResponse) -> bool {
    let r = unsafe { resp.response() };
    let url = response_url(&r);
    if !(url.starts_with("http:") || url.starts_with("https:")) {
        return false;
    }
    is_attachment(&disposition(&r)) || !unsafe { resp.canShowMIMEType() }
}

define_class!(
    /// `WKNavigationDelegate` that signals page-load completion.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "VsNavDelegate"]
    #[ivars = NavDelegateIvars]
    pub(super) struct NavDelegate;

    impl NavDelegate {
        #[unsafe(method(webView:didStartProvisionalNavigation:))]
        fn did_start(&self, _web_view: &WKWebView, _nav: Option<&WKNavigation>) {
            self.ivars().slot.borrow_mut().started = true;
            self.ivars().main_response.borrow_mut().take();
        }

        #[unsafe(method(webView:didCommitNavigation:))]
        fn did_commit(&self, _web_view: &WKWebView, _nav: Option<&WKNavigation>) {
            let mut slot = self.ivars().slot.borrow_mut();
            slot.started = true;
            slot.committed = true;
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, _web_view: &WKWebView, _nav: Option<&WKNavigation>) {
            let mut slot = self.ivars().slot.borrow_mut();
            slot.started = true;
            slot.committed = true;
            slot.done = Some(Ok(()));
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(
            &self,
            _web_view: &WKWebView,
            _nav: Option<&WKNavigation>,
            error: &NSError,
        ) {
            let msg = error.localizedDescription().to_string();
            self.ivars().slot.borrow_mut().done = Some(Err(msg));
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(
            &self,
            _web_view: &WKWebView,
            _nav: Option<&WKNavigation>,
            error: &NSError,
        ) {
            // A main-frame response that became a download ends the
            // provisional load with "frame load interrupted". The page
            // is unchanged and the file is on its way: not a failure.
            if self.ivars().main_became_download.replace(false) {
                self.ivars().slot.borrow_mut().done = Some(Ok(()));
                return;
            }
            let msg = error.localizedDescription().to_string();
            self.ivars().slot.borrow_mut().done = Some(Err(msg));
        }

        /// Save attachments instead of dropping them.
        ///
        /// Without this method WebKit cancels any response it cannot
        /// render, and a `Content-Disposition: attachment` response is
        /// lost with no event: an export button, a form POST that
        /// answers with a file, a hidden-iframe download.
        #[unsafe(method(webView:decidePolicyForNavigationResponse:decisionHandler:))]
        fn decide_response(
            &self,
            _web_view: &WKWebView,
            resp: &WKNavigationResponse,
            handler: &block2::DynBlock<dyn Fn(WKNavigationResponsePolicy)>,
        ) {
            let main = unsafe { resp.isForMainFrame() };
            let policy = if wants_download(resp) {
                if main {
                    self.ivars().main_became_download.set(true);
                }
                WKNavigationResponsePolicy::Download
            } else {
                if main {
                    let r = unsafe { resp.response() };
                    *self.ivars().main_response.borrow_mut() = http_response(&r);
                }
                WKNavigationResponsePolicy::Allow
            };
            handler.call((policy,));
        }

        /// Server trust for an intercept page: its TLS ends at the
        /// daemon's proxy, whose CA is trusted here (see `trust`).
        /// Everything else gets WebKit's default handling, the same as
        /// without this method. WebKit sends the challenge for every
        /// connection of the page, subresources included.
        #[unsafe(method(webView:didReceiveAuthenticationChallenge:completionHandler:))]
        fn auth_challenge(
            &self,
            _web_view: &WKWebView,
            challenge: &NSURLAuthenticationChallenge,
            handler: &block2::DynBlock<dyn Fn(NSInteger, *mut NSURLCredential)>,
        ) {
            // NSURLSessionAuthChallengeUseCredential = 0,
            // PerformDefaultHandling = 1.
            let cred = self
                .ivars()
                .trust_anchor
                .borrow()
                .as_deref()
                .and_then(|ca| super::trust::credential_for(challenge, ca));
            match cred {
                Some(c) => handler.call((0, Retained::as_ptr(&c).cast_mut())),
                None => handler.call((1, std::ptr::null_mut())),
            }
        }

        #[unsafe(method(webView:navigationResponse:didBecomeDownload:))]
        fn response_became_download(
            &self,
            _web_view: &WKWebView,
            resp: &WKNavigationResponse,
            download: &WKDownload,
        ) {
            let r = unsafe { resp.response() };
            let name = name_from_disposition(&disposition(&r))
                .unwrap_or_else(|| ns_str(r.suggestedFilename()));
            let id = match self.ivars().downloads.lock() {
                Ok(mut d) => d.begin(&response_url(&r), &name, &ns_str(r.MIMEType())),
                Err(_) => return,
            };
            let key = std::ptr::from_ref(download) as usize;
            self.ivars()
                .in_flight
                .borrow_mut()
                .push((key, id, temp_path(id)));
            unsafe { download.setDelegate(Some(ProtocolObject::from_ref(self))) };
        }

        /// Answer WebKit's request for the host window's frame.
        ///
        /// `window.outerWidth` / `outerHeight` / `screenX` / `screenY`
        /// are sourced from the UI client, not from the NSWindow. With
        /// no UI delegate installed WebKit had nothing to ask and the
        /// page saw a zero-sized outer window — which no real browser
        /// reports, and which gives nonsense to any responsive code
        /// deriving browser-chrome height from
        /// `outerHeight - innerHeight`.
        #[unsafe(method(_webView:getWindowFrameWithCompletionHandler:))]
        fn get_window_frame(
            &self,
            web_view: &WKWebView,
            handler: &block2::DynBlock<dyn Fn(NSRect)>,
        ) {
            let frame = web_view
                .window()
                .map_or_else(|| web_view.frame(), |w| w.frame());
            handler.call((frame,));
        }

        /// A `target="_blank"` link or form, or `window.open()`.
        ///
        /// A headless page has nowhere to put a second window, and
        /// without this method WebKit drops the navigation: the click
        /// does nothing and the agent sees an unchanged tree. Park the
        /// request instead; the input primitive that caused it loads it
        /// in this page and waits for it (see `follow_new_window`). A
        /// load started from inside this callback is dropped, because
        /// WebKit is still deciding the originating navigation. The
        /// request carries method and body, so a POSTed form (SAML and
        /// institution-login handoffs) arrives intact.
        #[unsafe(method(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        fn create_web_view(
            &self,
            _web_view: &WKWebView,
            _config: &WKWebViewConfiguration,
            action: &WKNavigationAction,
            _features: &WKWindowFeatures,
        ) -> *mut WKWebView {
            let request = unsafe { action.request() };
            *self.ivars().new_window.borrow_mut() = Some(request);
            std::ptr::null_mut()
        }
    }

    unsafe impl NSObjectProtocol for NavDelegate {}
    unsafe impl WKNavigationDelegate for NavDelegate {}
    unsafe impl WKUIDelegate for NavDelegate {}

    unsafe impl WKDownloadDelegate for NavDelegate {
        #[unsafe(method(download:decideDestinationUsingResponse:suggestedFilename:completionHandler:))]
        fn decide_destination(
            &self,
            download: &WKDownload,
            response: &NSURLResponse,
            suggested: &NSString,
            handler: &block2::DynBlock<dyn Fn(*mut NSURL)>,
        ) {
            let Some((id, path)) = self.in_flight_entry(download) else {
                handler.call((std::ptr::null_mut(),));
                return;
            };
            if let Ok(mut d) = self.ivars().downloads.lock() {
                let header = name_from_disposition(&disposition(response));
                if header.is_none() {
                    d.set_name(id, &suggested.to_string());
                }
            }
            let expected = response.expectedContentLength();
            if u64::try_from(expected).is_ok_and(|n| n > MAX_BYTES) {
                self.settle(download, Err(format!(
                    "download exceeds {MAX_BYTES} bytes ({expected})"
                )));
                handler.call((std::ptr::null_mut(),));
                return;
            }
            let _ = std::fs::remove_file(&path);
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            handler.call((Retained::as_ptr(&url).cast_mut(),));
        }

        #[unsafe(method(downloadDidFinish:))]
        fn download_did_finish(&self, download: &WKDownload) {
            let Some((_, path)) = self.in_flight_entry(download) else {
                return;
            };
            let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()));
            let _ = std::fs::remove_file(&path);
            self.settle(download, bytes);
        }

        #[unsafe(method(download:didFailWithError:resumeData:))]
        fn download_did_fail(
            &self,
            download: &WKDownload,
            error: &NSError,
            _resume: Option<&NSData>,
        ) {
            if let Some((_, path)) = self.in_flight_entry(download) {
                let _ = std::fs::remove_file(&path);
            }
            self.settle(download, Err(error.localizedDescription().to_string()));
        }
    }
);

impl NavDelegate {
    pub(super) fn new(
        mtm: MainThreadMarker,
        slot: NavSlot,
        downloads: SharedDownloads,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(NavDelegateIvars {
            slot,
            new_window: RefCell::new(None),
            downloads,
            in_flight: RefCell::new(Vec::new()),
            main_became_download: Cell::new(false),
            main_response: RefCell::new(None),
            trust_anchor: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Trust TLS leaves signed by `ca_der` (the intercept proxy's CA)
    /// for this page's connections.
    pub(super) fn set_trust_anchor(&self, ca_der: Vec<u8>) {
        *self.ivars().trust_anchor.borrow_mut() = Some(ca_der);
    }

    /// Shared navigation-result slot. Reused by `navigate` to await an
    /// in-place navigation on the same delegate.
    pub(super) fn slot(&self) -> NavSlot {
        self.ivars().slot.clone()
    }

    /// Downloads this page's responses turned into.
    pub(super) fn downloads(&self) -> SharedDownloads {
        self.ivars().downloads.clone()
    }

    /// HTTP response of the latest main-frame navigation, if any.
    pub(super) fn main_response(&self) -> Option<crate::inspector::MainResponse> {
        self.ivars().main_response.borrow().clone()
    }

    /// Whether the page has asked for a new window we have not loaded.
    pub(super) fn has_new_window(&self) -> bool {
        self.ivars().new_window.borrow().is_some()
    }

    fn in_flight_entry(&self, download: &WKDownload) -> Option<(u64, PathBuf)> {
        let key = std::ptr::from_ref(download) as usize;
        self.ivars()
            .in_flight
            .borrow()
            .iter()
            .find(|(k, _, _)| *k == key)
            .map(|(_, id, p)| (*id, p.clone()))
    }

    /// Record the outcome and forget the `WKDownload`.
    fn settle(&self, download: &WKDownload, result: Result<Vec<u8>, String>) {
        let key = std::ptr::from_ref(download) as usize;
        let mut in_flight = self.ivars().in_flight.borrow_mut();
        let Some(pos) = in_flight.iter().position(|(k, _, _)| *k == key) else {
            return;
        };
        let (_, id, _) = in_flight.remove(pos);
        if let Ok(mut d) = self.ivars().downloads.lock() {
            d.finish(id, result);
        }
    }

    /// Take the parked new-window request, if the page asked for one.
    pub(super) fn take_new_window(&self) -> Option<Retained<NSURLRequest>> {
        self.ivars().new_window.borrow_mut().take()
    }
}

/// How long a web-content process may take to accept the first load.
///
/// This is not the page-load budget. A cold `WKWebView` (first open
/// after the daemon starts, or the first open after the process pool
/// has been starved) spends that time in XPC before `didStart` fires.
/// Charging it against the 15s load budget made `open` return
/// `! TIMEOUT 15000ms open` for a navigation that had not begun. 45s
/// is the allowance the integration harness already had to grant the
/// whole open for the same reason.
const LAUNCH_BUDGET: Duration = Duration::from_secs(45);

/// Wait until `slot` finishes, fails, or the load budget expires.
///
/// The load budget starts at provisional navigation, not at
/// `loadRequest`. If the document has committed and subresources then
/// hang, the page is usable and this returns `Ok` — a tracker that
/// never completes is not a failed open.
pub(super) fn wait_for_navigation(
    slot: &NavSlot,
    load_budget: Duration,
    primitive: &'static str,
) -> crate::engine::EngineResult<()> {
    use crate::engine::EngineError;

    use super::eval::run_loop_until;

    let launched = run_loop_until(
        || {
            let state = slot.borrow();
            state.started || state.committed || state.done.is_some()
        },
        LAUNCH_BUDGET,
    );
    if !launched {
        return Err(EngineError::Timeout {
            budget: LAUNCH_BUDGET,
            primitive,
            detail: "web process did not start",
        });
    }
    if let Some(result) = slot.borrow().done.clone() {
        return result.map_err(|msg| EngineError::Other(format!("navigation failed: {msg}")));
    }

    let _finished = run_loop_until(|| slot.borrow().done.is_some(), load_budget);
    let state = slot.borrow();
    match &state.done {
        Some(Ok(())) => Ok(()),
        Some(Err(msg)) => Err(EngineError::Other(format!("navigation failed: {msg}"))),
        // The main document is in. A subresource that never completes
        // (a tracker, an open websocket counted as a load) must not
        // fail the open — the page is readable.
        None if state.committed => Ok(()),
        None => Err(EngineError::Timeout {
            budget: load_budget,
            primitive,
            detail: "navigation did not finish",
        }),
    }
}
