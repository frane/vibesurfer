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
mod eval;
mod input;
mod inspector_handler;
mod nav_delegate;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSBackingStoreType, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSURLRequest, NSURL};
use objc2_web_kit::{WKNavigationDelegate, WKWebView, WKWebViewConfiguration};
use vs_protocol::{Ref, Tree};

use crate::engine::{
    ActTarget, Action, AuthBlob, CaptureScope, Engine, EngineCapabilities, EngineError,
    EngineResult, LayoutBox, PageHandle, Viewport, WaitCondition,
};

use eval::{eval_js_string, run_loop_until};
use nav_delegate::{NavDelegate, NavSlot};

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
    /// property on `WKWebView` is `weak`).
    _nav_delegate: Retained<NavDelegate>,
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
}

// =============================================================================
// Backend
// =============================================================================

/// Real WKWebView backend. Construct on the main thread; subsequent
/// calls assume main-thread context.
pub struct WkBackend {
    mtm: MainThreadMarker,
    pages: HashMap<PageHandle, WkPage>,
    next_handle: u64,
    captures_dir: Option<PathBuf>,
}

impl WkBackend {
    #[must_use]
    pub fn new(mtm: MainThreadMarker) -> Self {
        Self {
            mtm,
            pages: HashMap::new(),
            next_handle: 1,
            captures_dir: None,
        }
    }

    /// Pin the on-disk directory where `capture` writes PNGs. Defaults
    /// to a system temp subdirectory.
    #[must_use]
    pub fn with_capture_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.captures_dir = Some(dir.into());
        self
    }

    fn alloc_handle(&mut self) -> PageHandle {
        let h = PageHandle(self.next_handle);
        self.next_handle += 1;
        h
    }

    fn page_mut(&mut self, h: PageHandle) -> EngineResult<&mut WkPage> {
        self.pages.get_mut(&h).ok_or(EngineError::NotFound {
            kind: "page",
            id: h.0.to_string(),
        })
    }
}

// =============================================================================
// Shared payloads + parsers
// =============================================================================

use std::cell::RefCell;
use std::rc::Rc;

use super::common::{parse_snapshot, SNAPSHOT_DOM_WALKER_JS as SNAPSHOT_JS};

// =============================================================================
// Engine impl
// =============================================================================

/// Stable seed for a per-ref Bezier path. Same ref → same seed → same
/// path on every act. Different refs vary so an agent clicking through
/// a list doesn't draw an identical curve N times.
#[allow(clippy::cast_lossless)]
fn vs_humanize_seed_for_ref(r: vs_protocol::Ref) -> u64 {
    (u64::from(r.0)).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xDEAD_BEEF_CAFE_BABE
}

impl Engine for WkBackend {
    fn open(&mut self, url: &str) -> EngineResult<PageHandle> {
        let mtm = self.mtm;

        let inspector =
            super::inspector_bridge::InspectorSlots::new(crate::inspector::DEFAULT_BUFFER_CAPACITY);
        let config = unsafe { WKWebViewConfiguration::new(mtm) };
        // Pin the shared persistent data store explicitly. The default
        // value of `websiteDataStore` is *supposed* to be the persistent
        // default, but on headless Cocoa processes the binding can land
        // on a non-persistent store and any `Set-Cookie` from in-session
        // fetches gets silently dropped between the network and the
        // cookie jar. Assigning `defaultDataStore` defensively pins it.
        let data_store = unsafe { objc2_web_kit::WKWebsiteDataStore::defaultDataStore(mtm) };
        unsafe { config.setWebsiteDataStore(&data_store) };
        let ucc = unsafe { config.userContentController() };
        let inspector_installed = inspector_handler::install(mtm, &ucc, &inspector);
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1280.0, 800.0));
        let web_view: Retained<WKWebView> = unsafe {
            WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config)
        };

        // Host the webview in an offscreen NSWindow so synthesized
        // `NSEvent`s have a responder chain to traverse. Borderless
        // style with `Buffered` backing keeps the window cheap and
        // avoids any visible decoration; `setReleasedWhenClosed`
        // false because we manage the Retained handle ourselves.
        let window: Retained<NSWindow> = unsafe {
            let w = NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            );
            w.setReleasedWhenClosed(false);
            w.setContentView(Some(&web_view));
            w
        };

        // Set a current Safari user agent. Default WKWebView UA is
        // `Mozilla/5.0 (...) AppleWebKit/605.1.15 (KHTML, like Gecko)`
        // — missing the `Version/X Safari/X` suffix that real Safari
        // tacks on. Sites that fingerprint UAs flag this immediately
        // (Google's anti-bot is a regular offender). The string here
        // matches a recent shipping Safari; override per-page later
        // if you need to spoof something else.
        let ua = NSString::from_str(crate::engine::DEFAULT_USER_AGENT);
        unsafe { web_view.setCustomUserAgent(Some(&ua)) };

        let slot: NavSlot = Rc::new(RefCell::new(None));
        let delegate = NavDelegate::new(mtm, slot.clone());
        let proto: &ProtocolObject<dyn WKNavigationDelegate> = ProtocolObject::from_ref(&*delegate);
        unsafe { web_view.setNavigationDelegate(Some(proto)) };

        let ns_url_str = NSString::from_str(url);
        let ns_url = NSURL::URLWithString(&ns_url_str)
            .ok_or_else(|| EngineError::Other(format!("invalid url: {url}")))?;
        let request = NSURLRequest::requestWithURL(&ns_url);
        let _ = unsafe { web_view.loadRequest(&request) };

        let slot_check = slot.clone();
        let ok = run_loop_until(
            move || slot_check.borrow().is_some(),
            Duration::from_secs(15),
        );
        if !ok {
            return Err(EngineError::Timeout {
                budget: Duration::from_secs(15),
                primitive: "open",
            });
        }
        match slot.borrow_mut().take() {
            Some(Ok(())) => {}
            Some(Err(msg)) => return Err(EngineError::Other(format!("navigation failed: {msg}"))),
            None => unreachable!(),
        }

        let handle = self.alloc_handle();
        self.pages.insert(
            handle,
            WkPage {
                web_view,
                window,
                _nav_delegate: delegate,
                inspector,
                inspector_installed,
                cookie_baseline: std::cell::RefCell::new(None),
                cookie_next_seq: std::cell::RefCell::new(0),
                last_mouse: std::cell::Cell::new(vs_humanize::Point { x: 0.0, y: 0.0 }),
            },
        );
        Ok(handle)
    }

    fn close(&mut self, page: PageHandle) -> EngineResult<()> {
        self.pages.remove(&page);
        Ok(())
    }

    fn snapshot(&mut self, page: PageHandle) -> EngineResult<Tree> {
        let p = self.page_mut(page)?;
        let json = eval_js_string(&p.web_view, SNAPSHOT_JS, Duration::from_secs(5))?;
        parse_snapshot(&json).map_err(EngineError::Other)
    }

    fn act(&mut self, page: PageHandle, target: ActTarget, action: Action) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        let window = p.window.clone();

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
            let frame = web_view.frame();
            let start = p.last_mouse.get();
            // Default to Human mode for now; v0.1.8 follow-up surfaces a
            // `--mode={human,careful,robotic}` flag on the wire.
            let mode = vs_humanize::InputMode::Human;
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
            return Ok(());
        }

        super::common::run_act(
            move |js, budget| eval_js_string(&web_view, js, budget),
            &target,
            &action,
        )
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

    fn capture(&mut self, page: PageHandle, _scope: CaptureScope) -> EngineResult<PathBuf> {
        let captures_dir = self.captures_dir.clone();
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        capture::capture_to_png(&web_view, page, captures_dir.as_deref())
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
        let storage = super::common::run_save_storage_only(move |js, budget| {
            eval_js_string(&web_view, js, budget)
        })?;
        let blob = super::auth::AuthBlobV2 {
            version: 2,
            url: storage.url,
            origin: storage.origin,
            cookies,
            local_storage: storage.local_storage,
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
