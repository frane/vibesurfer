//! macOS WebKit backend — real `WKWebView` driven via `objc2`.
//!
//! `WKWebView` is hard-pinned to the Cocoa main thread. Every public
//! method on [`WkBackend`] requires that the caller be on the main
//! thread; the [`MainThreadMarker`] taken at construction is the
//! type-system proof of that. Production wiring lives in
//! `vs-cli::serve` (the `cfg(target_os = "macos")` path), which puts
//! `NSApplication` on the OS main thread and dispatches engine calls
//! onto it via [`crate::runtime::MainThreadDispatcher`].
//!
//! Module layout:
//! - [`nav_delegate`] — custom `WKNavigationDelegate` Obj-C class.
//! - [`eval`] — main-thread run-loop pump + `evaluateJavaScript`.
//! - [`capture`] — `takeSnapshotWithConfiguration:` → PNG → disk.
//! - [`engine_impl`] — the [`Engine`] impl for [`WkBackend`].
//! - [`popups`] — windows a page opens, queued for adoption.
//! - Shared primitive logic (act / wait / layout / save_auth /
//!   load_auth) lives in [`super::common`].

mod capture;
mod cookie_store;
mod dialogs;
mod drop;
mod engine_impl;
mod eval;
mod frames;
mod headless_webview;
mod headless_window;
mod input;
mod inspector_handler;
mod loopback;
mod nav_delegate;
mod popups;
mod proxy;
mod record;
mod trust;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSBackingStoreType, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_web_kit::{
    WKNavigationDelegate, WKUIDelegate, WKUserContentController, WKWebView, WKWebViewConfiguration,
};

use crate::engine::{CursorOp, DataStore, Engine, EngineError, EngineResult, PageHandle};

use eval::{eval_js_string, run_loop_until};
use nav_delegate::{wait_for_navigation, NavDelegate, NavSlot, NavState};

use super::common::NO_NATIVE_DRAG_JS;
use super::native_download::SharedDownloads;

use popups::{popup_closed, spawn_popup, PopupQueue};

// =============================================================================
// Per-page state
// =============================================================================
struct WkPage {
    web_view: Retained<WKWebView>,
    /// Offscreen NSWindow that hosts `web_view` as its content view.
    /// Required for trusted-event dispatch: synthesized `NSEvent`s
    /// route through `NSWindow::sendEvent` → responder chain →
    /// WKWebView, where WebKit promotes them to JS events with
    /// `event.isTrusted = true`. A free-floating WKWebView (no
    /// hosting window) silently drops mouse events because there's
    /// no responder chain for the event to traverse.
    window: Retained<NSWindow>,
    /// Owned so the webview keeps a strong reference (the delegate
    /// property on `WKWebView` is `weak`). Its nav slot is reused by
    /// `navigate` to wait for in-place navigations.
    nav_delegate: Retained<NavDelegate>,
    /// The page's user-content controller, kept so `enable_webauthn`
    /// can install a document-start user script that survives
    /// navigation (adding to a live UCC applies to subsequent loads).
    ucc: Retained<WKUserContentController>,
    /// Console / network ring buffers + request-detail map. Populated
    /// by the JS bridge installed at construction time.
    inspector: super::inspector_bridge::InspectorSlots,
    /// Whether the inspector install path actually succeeded for this
    /// page. Read by `Engine::capabilities()` and the daemon's
    /// `vs_inspect` gate; if false, the wire returns
    /// `! ENGINE_UNSUPPORTED <op>` instead of an empty buffer.
    inspector_installed: bool,
    /// Cookie snapshot from the last `cookie_events` call (or `None`
    /// until the first call). Diffed by `Engine::cookie_events` to
    /// surface ADDed and REMOVEd entries.
    cookie_baseline: std::cell::RefCell<Option<Vec<super::auth::CookieData>>>,
    /// Per-page monotonic counter for cookie-event sequence numbers.
    cookie_next_seq: std::cell::RefCell<u64>,
    /// Last known mouse position in CSS pixels (client-space top-left
    /// origin). Updated after each humanized act dispatch; used as the
    /// path start for the next move. Initial value (0,0) is the
    /// page-load default before the agent has moved the pointer.
    last_mouse: std::cell::Cell<vs_humanize::Point>,
    /// Subframes that reported in, for `eval_js_in_frame`. Shared with
    /// popups, which share the content controller.
    frames: frames::FrameList,
    /// The intercept proxy's port, for pages of a proxied store.
    proxy_port: Option<u16>,
    /// The web view sends its http(s) through [`loopback`]'s handler:
    /// the page is served from loopback, which WebKit never proxies.
    loopback: bool,
    /// The page's configuration without a loopback handler, which a
    /// rebuilt web view copies.
    base_config: Retained<WKWebViewConfiguration>,
}

// =============================================================================
// Backend
// =============================================================================

/// Real WKWebView backend. Construct on the main thread; subsequent
/// calls assume main-thread context.
pub struct WkBackend {
    mtm: MainThreadMarker,
    pages: HashMap<PageHandle, WkPage>,
    captures_dir: Option<PathBuf>,
    /// Active recording sinks by page. Present only while a `record`
    /// session is running; the input primitives grab frames through it.
    recording: HashMap<PageHandle, record::RecSink>,
    /// Non-persistent data stores for isolated sessions, by key. One
    /// instance per key so every page of the session shares it.
    isolated_stores: HashMap<String, Retained<objc2_web_kit::WKWebsiteDataStore>>,
    /// Windows pages with real popups opened, and pages that closed
    /// themselves, not yet adopted or released.
    popup_queue: PopupQueue,
    /// Per proxied data store (by address): whether its last
    /// navigation went to a loopback host. See `wants_loopback`.
    loopback_hint: HashMap<usize, bool>,
}

impl WkBackend {
    #[must_use]
    pub fn new(mtm: MainThreadMarker) -> Self {
        Self {
            mtm,
            pages: HashMap::new(),
            captures_dir: None,
            recording: HashMap::new(),
            isolated_stores: HashMap::new(),
            popup_queue: PopupQueue::default(),
            loopback_hint: HashMap::new(),
        }
    }

    /// Pin the on-disk directory where `capture` writes PNGs. Defaults
    /// to a system temp subdirectory.
    #[must_use]
    pub fn with_capture_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.captures_dir = Some(dir.into());
        self
    }

    /// The data store a new page opens in.
    ///
    /// Shared pages pin the persistent default explicitly. The default
    /// value of `websiteDataStore` is *supposed* to be the persistent
    /// default, but on headless Cocoa processes the binding can land
    /// on a non-persistent store and any `Set-Cookie` from in-session
    /// fetches gets silently dropped between the network and the
    /// cookie jar. Assigning `defaultDataStore` defensively pins it.
    ///
    /// An isolated session gets its own in-memory store instead,
    /// created on its first page and reused for the rest. A proxied
    /// one is the same, with the proxy configured before any page uses
    /// it.
    fn data_store(
        &mut self,
        store: &DataStore,
    ) -> EngineResult<Retained<objc2_web_kit::WKWebsiteDataStore>> {
        let mtm = self.mtm;
        let Some(key) = store.key() else {
            return Ok(unsafe { objc2_web_kit::WKWebsiteDataStore::defaultDataStore(mtm) });
        };
        if let Some(s) = self.isolated_stores.get(key) {
            return Ok(s.clone());
        }
        let s = unsafe { objc2_web_kit::WKWebsiteDataStore::nonPersistentDataStore(mtm) };
        if let Some(port) = store.proxy_port() {
            proxy::set_http_proxy(&s, port)?;
        }
        self.isolated_stores.insert(key.to_string(), s.clone());
        Ok(s)
    }

    /// Whether a proxied page opening at `url` gets a loopback web view.
    /// An http(s) URL decides by its host. A blank page (a context's
    /// first page, opened ahead of its first `goto`) takes the kind the
    /// store's last navigation needed, loopback before there was one:
    /// a test suite mostly runs against one local server, so the
    /// first navigation rarely has to rebuild the view.
    fn wants_loopback(&mut self, url: &str, store: usize) -> bool {
        if url.starts_with("http://") || url.starts_with("https://") {
            let want = loopback::is_loopback(url);
            self.loopback_hint.insert(store, want);
            want
        } else {
            self.loopback_hint.get(&store).copied().unwrap_or(true)
        }
    }

    /// Swap `page`'s web view for one of the other kind (loopback
    /// handler or proxy), in the same window, with the same data store,
    /// content controller, delegate and size. Init scripts, message
    /// handlers, popup and dialog settings carry over; the old view's
    /// history does not.
    fn rebuild_view(
        &mut self,
        page: PageHandle,
        loopback_view: bool,
        port: u16,
    ) -> EngineResult<()> {
        let mtm = self.mtm;
        let p = self.page_mut(page)?;
        let old = p.web_view.clone();
        let config: Retained<WKWebViewConfiguration> =
            unsafe { objc2::msg_send![&*p.base_config, copy] };
        if loopback_view {
            loopback::install(mtm, &config, port);
        }
        let web_view = headless_webview::HeadlessWebView::create(mtm, old.frame(), &config);
        unsafe {
            web_view.setCustomUserAgent(old.customUserAgent().as_deref());
            let proto: &ProtocolObject<dyn WKNavigationDelegate> =
                ProtocolObject::from_ref(&*p.nav_delegate);
            web_view.setNavigationDelegate(Some(proto));
            let ui: &ProtocolObject<dyn WKUIDelegate> = ProtocolObject::from_ref(&*p.nav_delegate);
            web_view.setUIDelegate(Some(ui));
            old.stopLoading();
            old.setNavigationDelegate(None);
            old.setUIDelegate(None);
            p.window.setContentView(Some(&web_view));
            p.window.makeFirstResponder(Some(&web_view));
        }
        p.web_view = web_view;
        p.loopback = loopback_view;
        Ok(())
    }

    fn alloc_handle(&mut self) -> PageHandle {
        self.popup_queue.borrow_mut().alloc()
    }

    /// Take in the windows pages opened and release the pages that
    /// closed themselves since the last call. Runs before any page
    /// lookup, so a popup's handle works as soon as it is reported.
    fn sync_popups(&mut self) {
        let (born, closed) = {
            let mut q = self.popup_queue.borrow_mut();
            (std::mem::take(&mut q.born), std::mem::take(&mut q.closed))
        };
        for b in born {
            // The opener's inspector buffers and content controller:
            // the popup shares its configuration, so its console and
            // scripts land there too.
            let (inspector, inspector_installed, ucc, frames, proxy_port, loopback, base) =
                match b.opener.and_then(|h| self.pages.get(&h)) {
                    Some(o) => (
                        o.inspector.clone(),
                        o.inspector_installed,
                        o.ucc.clone(),
                        o.frames.clone(),
                        o.proxy_port,
                        o.loopback,
                        Some(o.base_config.clone()),
                    ),
                    None => (
                        super::inspector_bridge::InspectorSlots::new(
                            crate::inspector::DEFAULT_BUFFER_CAPACITY,
                        ),
                        false,
                        unsafe { b.web_view.configuration().userContentController() },
                        frames::FrameList::default(),
                        None,
                        false,
                        None,
                    ),
                };
            let base_config = base
                .unwrap_or_else(|| unsafe { objc2::msg_send![&*b.web_view.configuration(), copy] });
            self.pages.insert(
                b.handle,
                WkPage {
                    web_view: b.web_view,
                    window: b.window,
                    nav_delegate: b.delegate,
                    ucc,
                    inspector,
                    inspector_installed,
                    cookie_baseline: std::cell::RefCell::new(None),
                    cookie_next_seq: std::cell::RefCell::new(0),
                    last_mouse: std::cell::Cell::new(vs_humanize::Point { x: 0.0, y: 0.0 }),
                    frames: frames.clone(),
                    proxy_port,
                    loopback,
                    base_config,
                },
            );
        }
        for h in closed {
            let _ = self.close(h);
        }
    }

    fn page_mut(&mut self, h: PageHandle) -> EngineResult<&mut WkPage> {
        self.sync_popups();
        self.pages.get_mut(&h).ok_or(EngineError::NotFound {
            kind: "page",
            id: h.0.to_string(),
        })
    }
}

/// Stop the load and break the retain cycle that keeps a web-content
/// process alive after the Rust handles drop. Leaked offscreen windows
/// and their WebKit processes pile up across a long session and
/// eventually starve new navigations (`could not connect to the
/// server`). Apple documents removing the script message handlers
/// explicitly; without that, every open/close leaked a render process
/// (~90 MB).
fn abandon_view(web_view: &WKWebView, window: &NSWindow, ucc: &WKUserContentController) {
    unsafe {
        ucc.removeAllScriptMessageHandlers();
        ucc.removeAllUserScripts();
        web_view.stopLoading();
        web_view.setNavigationDelegate(None);
        window.setContentView(None);
        window.close();
    }
}

/// [`abandon_view`] for a page whose user content controller another
/// page still uses (a popup and its opener).
fn abandon_view_keep_ucc(web_view: &WKWebView, window: &NSWindow) {
    unsafe {
        web_view.stopLoading();
        web_view.setNavigationDelegate(None);
        web_view.setUIDelegate(None);
        window.setContentView(None);
        window.close();
    }
}

/// Create a web view of `size` for `config` in an offscreen host
/// window, with a Safari user agent and its navigation/UI delegate.
/// `ca` is the intercept proxy's CA to trust, for proxied pages.
fn host_view(
    mtm: MainThreadMarker,
    config: &WKWebViewConfiguration,
    size: NSSize,
    ca: Option<Vec<u8>>,
) -> (
    Retained<WKWebView>,
    Retained<NSWindow>,
    Retained<NavDelegate>,
) {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), size);
    // VsHeadlessWebView refuses system drag sessions; see
    // headless_webview.rs.
    let web_view = headless_webview::HeadlessWebView::create(mtm, frame, config);

    // Host the webview in an offscreen NSWindow so synthesized
    // `NSEvent`s have a responder chain to traverse. Borderless
    // style with `Buffered` backing keeps the window cheap and
    // avoids any visible decoration; `setReleasedWhenClosed`
    // false because we manage the Retained handle ourselves.
    let window: Retained<NSWindow> = unsafe {
        // VsHeadlessWindow reports itself visible/unoccluded so the
        // headless page's timers + requestAnimationFrame keep running
        // (see headless_window.rs); it is still never ordered on
        // screen, so nothing is shown.
        let w = headless_window::HeadlessWindow::host(
            mtm,
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
        );
        w.setReleasedWhenClosed(false);
        w.setContentView(Some(&web_view));
        // Give the web view first-responder status. Together with
        // the window's `isKeyWindow` override this is what makes
        // `document.hasFocus()` true, so the page does not think it
        // is a backgrounded tab.
        w.makeFirstResponder(Some(&web_view));
        w
    };

    // Set a current Safari user agent. Default WKWebView UA is
    // `Mozilla/5.0 (...) AppleWebKit/605.1.15 (KHTML, like Gecko)`,
    // missing the `Version/X Safari/X` suffix that real Safari tacks
    // on. Sites that fingerprint UAs flag this immediately (Google's
    // anti-bot is a regular offender).
    let ua = NSString::from_str(crate::engine::DEFAULT_USER_AGENT);
    unsafe { web_view.setCustomUserAgent(Some(&ua)) };

    let slot: NavSlot = Rc::new(RefCell::new(NavState::default()));
    let delegate = NavDelegate::new(mtm, slot, SharedDownloads::default());
    if let Some(ca) = ca {
        delegate.set_trust_anchor(ca);
    }
    let proto: &ProtocolObject<dyn WKNavigationDelegate> = ProtocolObject::from_ref(&*delegate);
    unsafe { web_view.setNavigationDelegate(Some(proto)) };
    // Same object serves as the UI delegate: WebKit sources
    // `window.outerWidth` / `outerHeight` from the UI client, and
    // with none installed the page saw a zero-sized outer window.
    let ui_proto: &ProtocolObject<dyn WKUIDelegate> = ProtocolObject::from_ref(&*delegate);
    unsafe { web_view.setUIDelegate(Some(ui_proto)) };
    (web_view, window, delegate)
}

impl Drop for WkBackend {
    fn drop(&mut self) {
        // On daemon shutdown, tear every open page down and pump the run
        // loop so WebKit actually terminates its web-content processes
        // (~90 MB each) instead of orphaning them to launchd. `close`
        // detaches the message handlers / delegate and closes the host
        // window; the pump gives WebKit the main-thread turns it needs to
        // reap the process before the daemon exits. This only runs if the
        // backend is genuinely dropped — see the ordering in
        // `vs-cli::serve` that lets the engine channel close on shutdown.
        let handles: Vec<PageHandle> = self.pages.keys().copied().collect();
        for h in handles {
            let _ = self.close(h);
        }
        for _ in 0..20 {
            let _ = run_loop_until(|| false, Duration::from_millis(25));
        }
    }
}

/// Turn on `navigator.mediaDevices`.
///
/// WKWebView gates the whole MediaDevices surface behind a preference
/// that is off by default, so the object was simply absent — even on
/// https with `isSecureContext: true`, where every real browser has
/// it. Anything feature-detecting camera/mic support saw a browser
/// that cannot exist.
///
/// The toggle has no public API, so it goes through KVC on
/// WKPreferences. Unknown keys raise an Obj-C exception rather than
/// returning an error, and the key set drifts between OS releases, so
/// each is attempted inside a catch: a preference this macOS does not
/// have must not stop the browser from starting.
fn enable_media_devices(config: &WKWebViewConfiguration) {
    unsafe {
        let prefs = config.preferences();
        for key in ["mediaDevicesEnabled", "mediaStreamEnabled"] {
            let k = NSString::from_str(key);
            let yes = objc2_foundation::NSNumber::numberWithBool(true);
            let attempt = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
                let _: () = objc2::msg_send![&*prefs, setValue: &*yes, forKey: &*k];
            }));
            if attempt.is_err() {
                eprintln!("vs: WKPreferences key {key:?} unavailable on this macOS");
            }
        }
    }
}

/// Stable seed for a per-ref Bezier path. Same ref → same seed → same
/// path on every act. Different refs vary so an agent clicking through
/// a list doesn't draw an identical curve N times.
#[allow(clippy::cast_lossless)]
fn vs_humanize_seed_for_ref(r: vs_protocol::Ref) -> u64 {
    (u64::from(r.0)).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xDEAD_BEEF_CAFE_BABE
}

#[allow(clippy::cast_possible_truncation)]
fn vs_humanize_seed_for_xy(op: CursorOp) -> u64 {
    let (a, b) = match op {
        CursorOp::MoveTo { x, y }
        | CursorOp::HoverAt { x, y }
        | CursorOp::ClickAt { x, y }
        | CursorOp::Wheel { x, y, .. }
        | CursorOp::Click { x, y, .. } => (x, y),
        CursorOp::Drag { x1, y1, .. } => (x1, y1),
    };
    let xi = a.to_bits();
    let yi = b.to_bits();
    xi.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ yi
}

/// Document-start shim that lets `vs` drive `requestAnimationFrame`
/// callbacks on demand. A headless macOS WKWebView reports the page
/// hidden, so the web-content process pauses rAF — and rAF-deferred
/// teardown in libraries (Floating UI popper unmount, rAF-based
/// scroll-lock release) never runs, wedging the page after a Select/menu
/// commits. The shim queues every rAF callback (still forwarding to the
/// real rAF, so on platforms where it fires nothing changes) and exposes
/// `window.__vsFlushRAF()`, which `act` calls afterward to run any
/// callbacks the suspended frame clock left pending. Same-callback double
/// runs are guarded by per-record `done`/`cancelled` flags.
const RAF_SHIM_JS: &str = r"
(function () {
  if (window.__vsRAFShimmed) return;
  window.__vsRAFShimmed = true;
  var realRAF = window.requestAnimationFrame ? window.requestAnimationFrame.bind(window) : null;
  var pending = new Map();
  var nextId = 1;
  function run(id, ts) {
    var rec = pending.get(id);
    if (!rec || rec.done || rec.cancelled) return;
    rec.done = true;
    pending.delete(id);
    try { rec.cb(ts); } catch (e) {}
  }
  window.requestAnimationFrame = function (cb) {
    var id = nextId++;
    pending.set(id, { cb: cb, done: false, cancelled: false });
    if (realRAF) realRAF(function (ts) { run(id, ts); });
    return id;
  };
  window.cancelAnimationFrame = function (id) {
    var rec = pending.get(id);
    if (rec) { rec.cancelled = true; pending.delete(id); }
  };
  window.__vsFlushRAF = function (maxRounds) {
    maxRounds = maxRounds || 5;
    var total = 0;
    for (var r = 0; r < maxRounds && pending.size > 0; r++) {
      var ts = (window.performance && performance.now) ? performance.now() : Date.now();
      var ids = Array.from(pending.keys());
      for (var i = 0; i < ids.length; i++) { total++; run(ids[i], ts); }
    }
    return total;
  };
})();
";

impl WkBackend {
    /// Load a new-window navigation the page asked for during the last
    /// input primitive, in this page, and wait for it like `goto`.
    ///
    /// A `target="_blank"` link or form, or `window.open()`, has no
    /// second window to go to headless. The UI delegate parks the
    /// request (`NavDelegate::create_web_view`); without this the click
    /// returned ok and nothing happened. No parked request: no-op.
    /// Drop a new-window request parked before this input primitive
    /// started (a popup a page fired on its own), so the primitive only
    /// follows what it caused.
    fn forget_new_window(&mut self, page: PageHandle) -> EngineResult<()> {
        let _ = self.page_mut(page)?.nav_delegate.take_new_window();
        let raised = self.popup_queue.borrow().feed.raised();
        self.popup_queue.borrow_mut().raised_at_input = raised;
        Ok(())
    }

    ///
    /// WebKit can ask for the new window after the click has returned,
    /// so after a click (`settle`) pump for up to `NEW_WINDOW_SETTLE`
    /// first.
    fn follow_new_window(&mut self, page: PageHandle, settle: bool) -> EngineResult<()> {
        let (web_view, delegate) = {
            let p = self.page_mut(page)?;
            (p.web_view.clone(), p.nav_delegate.clone())
        };
        if settle && !delegate.has_new_window() {
            let hint = eval_js_string(
                &web_view,
                super::common::NEW_WINDOW_HINT_JS,
                Duration::from_secs(2),
            );
            if super::common::new_window_hinted(&hint) {
                if delegate.popups() {
                    // A real window: wait for WebKit to ask for it, so
                    // the popup exists when the click returns.
                    let queue = self.popup_queue.clone();
                    let feed = queue.borrow().feed.clone();
                    let before = queue.borrow().raised_at_input;
                    let _ = run_loop_until(
                        move || feed.raised() > before,
                        super::common::NEW_WINDOW_SETTLE,
                    );
                    return Ok(());
                }
                let waiting = delegate.clone();
                let _ = run_loop_until(
                    move || waiting.has_new_window(),
                    super::common::NEW_WINDOW_SETTLE,
                );
            }
        }
        let Some(request) = delegate.take_new_window() else {
            return Ok(());
        };
        let slot = delegate.slot();
        slot.borrow_mut().reset();
        let _ = unsafe { web_view.loadRequest(&request) };
        wait_for_navigation(&slot, super::common::nav_budget(), "act")
    }
}
