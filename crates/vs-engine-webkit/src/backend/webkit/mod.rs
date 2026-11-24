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
//! - Shared primitive logic (act / wait / layout / save_auth /
//!   load_auth) lives in [`super::common`].

mod capture;
mod cookie_store;
mod dialogs;
mod eval;
mod frames;
mod headless_webview;
mod headless_window;
mod input;
mod inspector_handler;
mod loopback;
mod nav_delegate;
mod proxy;
mod record;
mod trust;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSBackingStoreType, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSURLRequest, NSURL};
use objc2_web_kit::{
    WKNavigationDelegate, WKUIDelegate, WKUserContentController, WKUserScript,
    WKUserScriptInjectionTime, WKWebView, WKWebViewConfiguration,
};
use vs_protocol::{Ref, Tree};

use crate::engine::{
    ActTarget, Action, AuthBlob, CaptureScope, CursorOp, DataStore, Download, DownloadEntry,
    DownloadSource, Engine, EngineCapabilities, EngineError, EngineResult, InputMode, LayoutBox,
    PageHandle, Viewport, WaitCondition,
};

use eval::{eval_js_string, run_loop_until};
use nav_delegate::{wait_for_navigation, NavDelegate, NavSlot, NavState};

use super::native_download::SharedDownloads;

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
        let old_config = unsafe { old.configuration() };
        let config = unsafe { WKWebViewConfiguration::new(mtm) };
        unsafe {
            config.setWebsiteDataStore(&old_config.websiteDataStore());
            config.setUserContentController(&p.ucc);
            config.setPreferences(&old_config.preferences());
        }
        enable_media_devices(&config);
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
            let (inspector, inspector_installed, ucc, frames, proxy_port, loopback) =
                match b.opener.and_then(|h| self.pages.get(&h)) {
                    Some(o) => (
                        o.inspector.clone(),
                        o.inspector_installed,
                        o.ucc.clone(),
                        o.frames.clone(),
                        o.proxy_port,
                        o.loopback,
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
                    ),
                };
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

/// A window a page opened, waiting for the backend to adopt it.
pub(super) struct PopupBirth {
    handle: PageHandle,
    opener: Option<PageHandle>,
    web_view: Retained<WKWebView>,
    window: Retained<NSWindow>,
    delegate: Retained<NavDelegate>,
}

/// New windows and script-closed pages, for pages with real popups,
/// and the page-handle counter (a popup gets its handle at birth, so
/// the daemon can name it before the backend adopts it).
#[derive(Default)]
pub(super) struct PopupState {
    born: Vec<PopupBirth>,
    closed: Vec<PageHandle>,
    feed: std::sync::Arc<crate::engine::PopupFeed>,
    last_handle: u64,
    /// The feed count when the current input primitive started.
    raised_at_input: u64,
}

impl PopupState {
    fn alloc(&mut self) -> PageHandle {
        self.last_handle += 1;
        PageHandle(self.last_handle)
    }
}

pub(super) type PopupQueue = Rc<RefCell<PopupState>>;

/// Build the window WebKit asked for from `opener` (UI delegate
/// `createWebView`) at the opener's size, and report it. The
/// configuration is WebKit's: same process pool, data store and user
/// content controller, so the opener's scripts run in it too.
pub(super) fn spawn_popup(
    mtm: MainThreadMarker,
    opener: Option<PageHandle>,
    opener_view: &WKWebView,
    config: &WKWebViewConfiguration,
    url: String,
    ca: Option<Vec<u8>>,
    queue: &PopupQueue,
) -> Retained<WKWebView> {
    let size = opener_view.frame().size;
    let (web_view, window, delegate) = host_view(mtm, config, size, ca);
    let mut q = queue.borrow_mut();
    let handle = q.alloc();
    delegate.set_popups(Some(queue.clone()), Some(handle));
    q.born.push(PopupBirth {
        handle,
        opener,
        web_view: web_view.clone(),
        window,
        delegate,
    });
    q.feed
        .push(crate::engine::PopupEvent::Opened(crate::engine::Popup {
            opener,
            page: handle,
            url,
        }));
    web_view
}

/// `window.close()` from `page`: report it; the backend releases it.
pub(super) fn popup_closed(queue: &PopupQueue, page: PageHandle) {
    let mut q = queue.borrow_mut();
    q.closed.push(page);
    q.feed.push(crate::engine::PopupEvent::Closed(page));
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

// =============================================================================
// Shared payloads + parsers
// =============================================================================

use std::cell::RefCell;
use std::rc::Rc;

use super::common::{parse_snapshot, SNAPSHOT_DOM_WALKER_JS as SNAPSHOT_JS};

/// Virtual WebAuthn authenticator shim (see `webauthn_virtual.js`).
const WEBAUTHN_JS: &str = include_str!("../webauthn_virtual.js");

// =============================================================================
// Engine impl
// =============================================================================

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
        | CursorOp::Wheel { x, y, .. } => (x, y),
        CursorOp::Drag { x1, y1, .. } => (x1, y1),
    };
    let xi = a.to_bits();
    let yi = b.to_bits();
    xi.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ yi
}

use super::common::NO_NATIVE_DRAG_JS;

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

impl Engine for WkBackend {
    fn open(&mut self, url: &str, store: &DataStore) -> EngineResult<PageHandle> {
        let mtm = self.mtm;

        let inspector =
            super::inspector_bridge::InspectorSlots::new(crate::inspector::DEFAULT_BUFFER_CAPACITY);
        let config = unsafe { WKWebViewConfiguration::new(mtm) };
        let data_store = self.data_store(store)?;
        unsafe { config.setWebsiteDataStore(&data_store) };
        let proxy_port = store.proxy_port();
        let loopback = proxy_port.is_some()
            && self.wants_loopback(url, Retained::as_ptr(&data_store) as usize);
        if let (true, Some(port)) = (loopback, proxy_port) {
            loopback::install(mtm, &config, port);
        }
        // Expose `navigator.mediaDevices`. WKWebView gates the whole
        // MediaDevices surface behind a preference that is off by
        // default, so the object was simply absent — even on https with
        // `isSecureContext: true`, where every real browser has it.
        // Anything feature-detecting camera/mic support saw a browser
        // that cannot exist.
        //
        // The toggle has no public API, so it goes through KVC on
        // WKPreferences. Unknown keys raise an Obj-C exception rather
        // than returning an error, and the set of keys drifts between
        // OS releases, so each one is attempted inside a catch and a
        // failure is logged and ignored — a missing preference must not
        // stop the browser from starting.
        enable_media_devices(&config);

        let ucc = unsafe { config.userContentController() };
        // No native drags, in every frame (see NO_NATIVE_DRAG_JS).
        unsafe {
            let src = NSString::from_str(NO_NATIVE_DRAG_JS);
            let shim = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(mtm),
                &src,
                WKUserScriptInjectionTime::AtDocumentStart,
                false,
            );
            ucc.addUserScript(&shim);
        }
        // Inject the rAF-flush shim at document-start so `act` can drive
        // pending requestAnimationFrame callbacks even though a headless
        // macOS page has its frame clock paused (see RAF_SHIM_JS).
        unsafe {
            let src = NSString::from_str(RAF_SHIM_JS);
            let shim = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(mtm),
                &src,
                WKUserScriptInjectionTime::AtDocumentStart,
                false,
            );
            ucc.addUserScript(&shim);
        }
        // Download capture, in every frame (`forMainFrameOnly: false`):
        // a viewer that saves a blob usually lives in an iframe, and
        // WKWebView has no download delegate to catch it.
        unsafe {
            let src = NSString::from_str(super::common::DOWNLOAD_SHIM_JS);
            let shim = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(mtm),
                &src,
                WKUserScriptInjectionTime::AtDocumentStart,
                false,
            );
            ucc.addUserScript(&shim);
        }
        let inspector_installed = inspector_handler::install(mtm, &ucc, &inspector);
        let frames = frames::FrameList::default();
        frames::install(mtm, &ucc, &frames);
        // Default browser window: a natural 1440x900 (MacBook-class),
        // the logical size pages render at. Retina still doubles the
        // pixel density for screenshots; recordings capture at this size.
        let size = NSSize::new(1440.0, 900.0);
        let (web_view, window, delegate) =
            host_view(mtm, &config, size, store.proxy_ca().map(<[u8]>::to_vec));
        let slot = delegate.slot();

        let ns_url_str = NSString::from_str(url);
        let ns_url = NSURL::URLWithString(&ns_url_str)
            .ok_or_else(|| EngineError::Other(format!("invalid url: {url}")))?;
        let request = NSURLRequest::requestWithURL(&ns_url);
        let _ = unsafe { web_view.loadRequest(&request) };

        let budget = super::common::nav_budget();
        if let Err(e) = wait_for_navigation(&slot, budget, "open") {
            // Dropping the Retained handles does not reap the web
            // process — that leak is what later starves the next open
            // with "could not connect to the server". Tear down the
            // same way `close` does, then give WebKit a few turns to
            // actually exit the process.
            abandon_view(&web_view, &window, &ucc);
            for _ in 0..8 {
                let _ = run_loop_until(|| false, Duration::from_millis(25));
            }
            return Err(e);
        }

        let handle = self.alloc_handle();
        self.pages.insert(
            handle,
            WkPage {
                web_view,
                window,
                nav_delegate: delegate,
                ucc,
                inspector,
                inspector_installed,
                cookie_baseline: std::cell::RefCell::new(None),
                cookie_next_seq: std::cell::RefCell::new(0),
                last_mouse: std::cell::Cell::new(vs_humanize::Point { x: 0.0, y: 0.0 }),
                frames: frames.clone(),
                proxy_port,
                loopback,
            },
        );
        Ok(handle)
    }

    fn navigate(&mut self, page: PageHandle, url: &str) -> EngineResult<()> {
        // A proxied page moving between a loopback host and any other
        // needs the other kind of web view (see `loopback`).
        if let Some(port) = self.page_mut(page)?.proxy_port {
            if url.starts_with("http://") || url.starts_with("https://") {
                let want = loopback::is_loopback(url);
                let key = unsafe {
                    self.page_mut(page)?
                        .web_view
                        .configuration()
                        .websiteDataStore()
                };
                self.loopback_hint
                    .insert(Retained::as_ptr(&key) as usize, want);
                if want != self.page_mut(page)?.loopback {
                    self.rebuild_view(page, want, port)?;
                }
            }
        }
        // Reuse the existing web view, host window, and web-content
        // process: just reset the shared nav slot and load the new URL.
        // This skips the whole WKWebView / NSWindow / process spin-up
        // that dominates `open` (~90ms), leaving only nav + load.
        let (web_view, slot) = {
            let p = self.page_mut(page)?;
            (p.web_view.clone(), p.nav_delegate.slot())
        };
        slot.borrow_mut().reset();
        let ns_url_str = NSString::from_str(url);
        let ns_url = NSURL::URLWithString(&ns_url_str)
            .ok_or_else(|| EngineError::Other(format!("invalid url: {url}")))?;
        let request = NSURLRequest::requestWithURL(&ns_url);
        let _ = unsafe { web_view.loadRequest(&request) };
        wait_for_navigation(&slot, super::common::nav_budget(), "navigate")
    }

    fn load_html(&mut self, page: PageHandle, url: &str, html: &str) -> EngineResult<()> {
        let (web_view, slot) = {
            let p = self.page_mut(page)?;
            (p.web_view.clone(), p.nav_delegate.slot())
        };
        slot.borrow_mut().reset();
        let base = NSURL::URLWithString(&NSString::from_str(url))
            .ok_or_else(|| EngineError::Other(format!("invalid url: {url}")))?;
        let _ = unsafe { web_view.loadHTMLString_baseURL(&NSString::from_str(html), Some(&base)) };
        wait_for_navigation(&slot, super::common::nav_budget(), "load_html")
    }

    fn enable_webauthn(&mut self, page: PageHandle) -> EngineResult<()> {
        let mtm = self.mtm;
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        // Persist across navigations: add the shim as a document-start
        // user script on the live UCC (applies to subsequent loads).
        unsafe {
            let src = NSString::from_str(WEBAUTHN_JS);
            let shim = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(mtm),
                &src,
                WKUserScriptInjectionTime::AtDocumentStart,
                false,
            );
            p.ucc.addUserScript(&shim);
        }
        // Apply to the document already loaded, too.
        eval_js_string(&web_view, WEBAUTHN_JS, Duration::from_secs(5))?;
        Ok(())
    }

    fn add_init_script(&mut self, page: PageHandle, source: &str) -> EngineResult<()> {
        let mtm = self.mtm;
        let p = self.page_mut(page)?;
        unsafe {
            let src = NSString::from_str(source);
            let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(mtm),
                &src,
                WKUserScriptInjectionTime::AtDocumentStart,
                false,
            );
            p.ucc.addUserScript(&script);
        }
        Ok(())
    }

    fn close(&mut self, page: PageHandle) -> EngineResult<()> {
        if let Some(p) = self.pages.remove(&page) {
            // A popup shares its opener's user content controller; only
            // the last page using it may strip it.
            let shared = self
                .pages
                .values()
                .any(|o| Retained::as_ptr(&o.ucc) == Retained::as_ptr(&p.ucc));
            if shared {
                abandon_view_keep_ucc(&p.web_view, &p.window);
            } else {
                abandon_view(&p.web_view, &p.window, &p.ucc);
            }
        }
        Ok(())
    }

    fn set_popups(&mut self, page: PageHandle, on: bool) -> EngineResult<()> {
        let queue = on.then(|| self.popup_queue.clone());
        self.page_mut(page)?
            .nav_delegate
            .set_popups(queue, Some(page));
        Ok(())
    }

    fn popup_feed(&mut self) -> Option<std::sync::Arc<crate::engine::PopupFeed>> {
        Some(self.popup_queue.borrow().feed.clone())
    }

    fn drop_store(&mut self, key: &str) -> EngineResult<()> {
        // Releasing our reference lets WebKit free the in-memory store
        // once the last web view using it is gone.
        self.isolated_stores.remove(key);
        Ok(())
    }

    fn snapshot(&mut self, page: PageHandle) -> EngineResult<Tree> {
        let p = self.page_mut(page)?;
        let json = eval_js_string(&p.web_view, SNAPSHOT_JS, Duration::from_secs(5))?;
        parse_snapshot(&json).map_err(EngineError::Other)
    }

    fn act(
        &mut self,
        page: PageHandle,
        target: ActTarget,
        action: Action,
        mode: InputMode,
    ) -> EngineResult<()> {
        self.forget_new_window(page)?;
        let p = self.page_mut(page)?;

        let web_view = p.web_view.clone();
        let web_view_flush = p.web_view.clone();
        let window = p.window.clone();

        // After the action settles, drain any rAF callbacks the page
        // queued (e.g. Radix/Floating-UI teardown) — on a headless macOS
        // page the frame clock is paused, so without this the teardown
        // never runs and the page can wedge. No-op where rAF already
        // fired or the shim isn't present.
        // NB: eval_js_string treats the JS result as an NSString, so the
        // flush expression must evaluate to a string (a bare number
        // result would be mis-cast and abort).
        let flush = move |wv: &Retained<WKWebView>| {
            let _ = eval_js_string(
                wv,
                "String(window.__vsFlushRAF ? window.__vsFlushRAF() : 0)",
                Duration::from_secs(2),
            );
        };

        // Route plain `click` on a `Ref` target through native NSEvent
        // dispatch — produces JS events with `event.isTrusted = true`,
        // which anti-bot fingerprinters key off. Other actions
        // (`fill`, `scroll`, `key`, `submit`, `hover`, `focus`) keep
        // the JS-driven path: they're either text-content mutations
        // or they don't usually hit a trust check.
        if let (ActTarget::Ref(r), Action::Click) = (&target, &action) {
            let r = *r;
            let rect = input::ref_rect(&web_view, r)?.ok_or_else(|| EngineError::NotFound {
                kind: "ref",
                id: r.0.to_string(),
            })?;
            // Zero-size targets (hidden inputs, off-screen duplicates a
            // site keeps in the DOM — x.com login does this) cannot be
            // hit natively: the synthesized coordinates land at the
            // box origin and hit-test something else, so the click
            // ACKs and does nothing. The JS event path dispatches on
            // the element regardless of geometry, so fall through to
            // it instead of no-opping.
            if rect.width > 0.5 && rect.height > 0.5 {
                let frame = web_view.frame();
                let start = p.last_mouse.get();
                // Mode comes from the wire (-M/--mode). Ref-click
                // defaults to careful, a single-shot trusted move:
                // fast and still isTrusted. human is opt-in for
                // detector-scored flows, robotic teleports.
                let mode = match mode {
                    InputMode::Human => vs_humanize::InputMode::Human,
                    InputMode::Careful => vs_humanize::InputMode::Careful,
                    InputMode::Robotic => vs_humanize::InputMode::Robotic,
                };

                let seed = vs_humanize_seed_for_ref(r);
                let landed = input::click_at_rect(
                    &web_view,
                    &window,
                    rect,
                    frame.size.height,
                    start,
                    mode,
                    seed,
                )?;
                p.last_mouse.set(landed);
                flush(&web_view_flush);
                return self.follow_new_window(page, true);
            }
        }

        let result = super::common::run_act(
            move |js, budget| eval_js_string(&web_view, js, budget),
            &target,
            &action,
        );
        flush(&web_view_flush);
        result?;
        self.follow_new_window(page, super::common::may_open_window(&action))
    }

    fn wait(
        &mut self,
        page: PageHandle,
        cond: WaitCondition,
        budget: Duration,
    ) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_wait(
            |js, budget| eval_js_string(&web_view, js, budget),
            &cond,
            budget,
            || {
                let _ = run_loop_until(|| false, Duration::from_millis(50));
            },
        )
    }

    fn download(
        &mut self,
        page: PageHandle,
        source: DownloadSource,
        budget: Duration,
    ) -> EngineResult<Download> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        let native = p.nav_delegate.downloads();
        super::common::run_download(
            |js, budget| eval_js_string(&web_view, js, budget),
            || {
                let _ = run_loop_until(|| false, Duration::from_millis(50));
            },
            &source,
            budget,
            &native,
        )
    }

    fn download_list(&mut self, page: PageHandle) -> EngineResult<Vec<DownloadEntry>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        let native = p.nav_delegate.downloads();
        super::common::run_download_list(
            move |js, budget| eval_js_string(&web_view, js, budget),
            &native,
        )
    }

    fn capture(&mut self, page: PageHandle, _scope: CaptureScope) -> EngineResult<PathBuf> {
        let captures_dir = self.captures_dir.clone();
        let mtm = self.mtm;
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        // Screenshots keep full device resolution.
        capture::capture_to_png(&web_view, page, captures_dir.as_deref(), mtm, None)
    }

    fn capture_live(&mut self, page: PageHandle, max_width: u32) -> EngineResult<PathBuf> {
        let captures_dir = self.captures_dir.clone();
        let mtm = self.mtm;
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        // Live frames (watch / record) render smaller: faster to capture
        // and encode. `0` means full resolution.
        let width = (max_width > 0).then(|| f64::from(max_width));
        capture::capture_to_png(&web_view, page, captures_dir.as_deref(), mtm, width)
    }

    fn record_begin(
        &mut self,
        page: PageHandle,
        tx: std::sync::mpsc::Sender<crate::engine::RecFrame>,
        fps: u32,
        max_width: u32,
    ) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        let logical_width = p.web_view.frame().size.width;
        self.recording.insert(
            page,
            record::RecSink::new(tx, logical_width, max_width, fps),
        );
        Ok(())
    }

    fn record_end(&mut self, page: PageHandle) -> EngineResult<()> {
        self.recording.remove(&page);
        Ok(())
    }

    fn layout(&mut self, page: PageHandle, refs: &[Ref]) -> EngineResult<Vec<LayoutBox>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_layout(
            move |js, budget| eval_js_string(&web_view, js, budget),
            refs,
        )
    }

    fn set_viewport(&mut self, page: PageHandle, viewport: Viewport) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        let frame = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(f64::from(viewport.width), f64::from(viewport.height)),
        );
        p.web_view.setFrame(frame);
        Ok(())
    }

    fn save_auth(&mut self, page: PageHandle) -> EngineResult<AuthBlob> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        // Cookies: host-side via WKHTTPCookieStore so HttpOnly entries
        // are captured (the v0.1.1 document.cookie path silently
        // dropped them). localStorage/sessionStorage: JS shim, which
        // is the right surface for those.
        let cookies = cookie_store::get_all_cookies(&web_view)?;
        let storage = super::common::run_save_storage_only(
            move |js, budget| eval_js_string(&web_view, js, budget),
            || {
                let _ = run_loop_until(|| false, Duration::from_millis(50));
            },
        )?;
        let blob = super::auth::AuthBlobV2 {
            version: 2,
            url: storage.url,
            origin: storage.origin,
            cookies,
            local_storage: storage.local_storage,
            indexed_db: storage.indexed_db,
            indexed_db_skipped: storage.indexed_db_skipped,
            indexed_db_incomplete: storage.indexed_db_incomplete,
            session_storage: storage.session_storage,
        };
        super::auth::encode(&blob)
    }

    fn load_auth(&mut self, page: PageHandle, blob: &AuthBlob) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        let parsed = super::auth::decode(blob)?;
        // Cookies first so subsequent fetches by the storage-restore
        // JS (none today, but future hooks) see the auth state.
        cookie_store::set_cookies(&web_view, &parsed.cookies)?;
        super::common::run_load_storage_only(
            move |js, budget| eval_js_string(&web_view, js, budget),
            &parsed.local_storage,
            &parsed.session_storage,
            &parsed.indexed_db,
            || {
                let _ = run_loop_until(|| false, Duration::from_millis(50));
            },
        )
    }

    fn console_entries(
        &mut self,
        page: PageHandle,
    ) -> EngineResult<Vec<crate::inspector::ConsoleEntry>> {
        let p = self.page_mut(page)?;
        Ok(p.inspector.console.borrow().snapshot())
    }

    fn network_entries(
        &mut self,
        page: PageHandle,
    ) -> EngineResult<Vec<crate::inspector::NetworkEntry>> {
        let p = self.page_mut(page)?;
        Ok(p.inspector.network.borrow().snapshot())
    }

    fn request_detail(
        &mut self,
        page: PageHandle,
        seq: u64,
    ) -> EngineResult<Option<crate::inspector::RequestDetail>> {
        let p = self.page_mut(page)?;
        Ok(p.inspector.details.borrow().get(seq).cloned())
    }

    fn eval_js(
        &mut self,
        page: PageHandle,
        expr: &str,
    ) -> EngineResult<crate::inspector::EvalResult> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_eval(
            move |js, budget| eval_js_string(&web_view, js, budget),
            expr,
        )
    }

    fn eval_js_in_frame(
        &mut self,
        page: PageHandle,
        frame: &str,
        expr: &str,
    ) -> EngineResult<crate::inspector::EvalResult> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        let info =
            frames::find(&p.frames, &web_view, frame).ok_or_else(|| EngineError::NotFound {
                kind: "frame",
                id: frame.to_string(),
            })?;
        super::common::run_eval(
            move |js, budget| eval::eval_js_string_in(&web_view, Some(&info), js, budget),
            expr,
        )
    }

    fn storage(
        &mut self,
        page: PageHandle,
        scope: crate::inspector::StorageScope,
    ) -> EngineResult<Vec<crate::inspector::StorageEntry>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        // `document.cookie` is blind to HttpOnly cookies, so route the
        // cookie scope through the host-side cookie store the auth
        // save/load path already uses. Other scopes stay on JS.
        if matches!(scope, crate::inspector::StorageScope::Cookies) {
            let cookies = cookie_store::get_all_cookies(&web_view)?;
            return Ok(cookies
                .iter()
                .map(super::common::cookie_to_storage_entry)
                .collect());
        }
        super::common::run_storage(
            move |js, budget| eval_js_string(&web_view, js, budget),
            scope,
        )
    }

    fn scripts(&mut self, page: PageHandle) -> EngineResult<Vec<crate::inspector::ScriptEntry>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_scripts(move |js, budget| eval_js_string(&web_view, js, budget))
    }

    fn script_source(
        &mut self,
        page: PageHandle,
        seq: u64,
    ) -> EngineResult<Option<crate::inspector::ScriptSource>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_script_source(
            move |js, budget| eval_js_string(&web_view, js, budget),
            seq,
        )
    }

    fn dom(
        &mut self,
        page: PageHandle,
        r: Ref,
        extra_props: &[String],
    ) -> EngineResult<Option<crate::inspector::DomDetail>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_dom(
            move |js, budget| eval_js_string(&web_view, js, budget),
            r,
            extra_props,
        )
    }

    fn performance(
        &mut self,
        page: PageHandle,
    ) -> EngineResult<crate::inspector::PerformanceMetrics> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_performance(move |js, budget| eval_js_string(&web_view, js, budget))
    }

    fn main_response(
        &mut self,
        page: PageHandle,
    ) -> EngineResult<Option<crate::inspector::MainResponse>> {
        Ok(self.page_mut(page)?.nav_delegate.main_response())
    }

    fn cookie_events(
        &mut self,
        page: PageHandle,
    ) -> EngineResult<Vec<crate::inspector::CookieEvent>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        let current = cookie_store::get_all_cookies(&web_view)?;
        let previous = p.cookie_baseline.borrow().clone();
        let mut seq = p.cookie_next_seq.borrow_mut();
        let events = super::common::diff_cookies(previous.as_deref(), &current, &mut seq);
        *p.cookie_baseline.borrow_mut() = Some(current);
        Ok(events)
    }

    fn type_text(&mut self, page: PageHandle, text: &str, mode: InputMode) -> EngineResult<()> {
        let mtm = self.mtm;
        self.forget_new_window(page)?;
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        let web_view_flush = p.web_view.clone();
        let cursor = p.last_mouse.get();
        // Vary cadence per call so repeated typing is not identical.
        let seed = text
            .chars()
            .map(|c| c as u64)
            .fold(0x9e37_79b9_u64, |a, c| a.wrapping_mul(31).wrapping_add(c));
        let humanize_mode = match mode {
            InputMode::Human => vs_humanize::InputMode::Human,
            InputMode::Careful => vs_humanize::InputMode::Careful,
            InputMode::Robotic => vs_humanize::InputMode::Robotic,
        };
        let rec = self.recording.get(&page).map(|s| (s, mtm));
        input::type_text(&web_view, text, humanize_mode, seed, rec, cursor)?;
        // Drain rAF-deferred work (headless macOS pauses the frame
        // clock) so React state settles before the next primitive.
        let _ = eval_js_string(
            &web_view_flush,
            "String(window.__vsFlushRAF ? window.__vsFlushRAF() : 0)",
            Duration::from_secs(2),
        );
        self.follow_new_window(page, false)
    }

    fn cursor_op(&mut self, page: PageHandle, op: CursorOp, mode: InputMode) -> EngineResult<()> {
        let mtm = self.mtm;
        self.forget_new_window(page)?;
        let (web_view, window, webview_height, start) = {
            let p = self.page_mut(page)?;
            (
                p.web_view.clone(),
                p.window.clone(),
                p.web_view.frame().size.height,
                p.last_mouse.get(),
            )
        };
        let humanize_mode = match mode {
            InputMode::Human => vs_humanize::InputMode::Human,
            InputMode::Careful => vs_humanize::InputMode::Careful,
            InputMode::Robotic => vs_humanize::InputMode::Robotic,
        };
        let seed = vs_humanize_seed_for_xy(op);
        let rec = self.recording.get(&page).map(|s| (s, mtm));
        let landed = match op {
            CursorOp::MoveTo { x, y } | CursorOp::HoverAt { x, y } => input::move_along_path(
                &web_view,
                &window,
                webview_height,
                start,
                vs_humanize::Point { x, y },
                humanize_mode,
                seed,
                false,
                rec,
            )?,
            CursorOp::ClickAt { x, y } => input::click_at_xy(
                &web_view,
                &window,
                webview_height,
                start,
                vs_humanize::Point { x, y },
                humanize_mode,
                seed,
                rec,
            )?,
            CursorOp::Drag { x1, y1, x2, y2 } => {
                let landed = input::drag_xy(
                    &web_view,
                    &window,
                    webview_height,
                    vs_humanize::Point { x: x1, y: y1 },
                    vs_humanize::Point { x: x2, y: y2 },
                    humanize_mode,
                    seed,
                    rec,
                )?;
                // After the OS-level drag completes, fire the HTML5
                // DragEvent chain via JS so react-dnd / native HTML5 /
                // React-Flow HTML5-backend targets observe the drop.
                let html5_js = super::common::build_html5_drag_js(x1, y1, x2, y2);
                let _ = eval_js_string(&web_view, &html5_js, std::time::Duration::from_secs(2));
                landed
            }
            CursorOp::Wheel { x, y, dx, dy } => {
                let landed = input::move_along_path(
                    &web_view,
                    &window,
                    webview_height,
                    start,
                    vs_humanize::Point { x, y },
                    humanize_mode,
                    seed,
                    false,
                    rec,
                )?;
                input::wheel(&web_view, webview_height, landed, dx, dy)?;
                landed
            }
        };
        if let Some(p) = self.pages.get(&page) {
            p.last_mouse.set(landed);
        }
        self.follow_new_window(page, matches!(op, CursorOp::ClickAt { .. }))
    }
    fn capabilities(&self) -> EngineCapabilities {
        // Inspector console + network reflect actual install-path
        // success: a flag is `true` only if at least one currently-open
        // page successfully registered the JS bridge + script-message
        // handlers. The first page closed-and-removed flips the flag
        // back when no instances remain — so the daemon's `vs_inspect`
        // gate stays honest at all times.
        let any_inspector = self.pages.values().any(|p| p.inspector_installed);
        EngineCapabilities {
            renders: true,
            honors_viewport: true,
            measures_layout: true,
            persists_auth: true,
            inspector_console: any_inspector,
            inspector_network: any_inspector,
            inspector_cookie_events: true,
            name: "webkit",
            version: "macOS WebKit (objc2)",
        }
    }
}
