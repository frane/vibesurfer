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
mod eval;
mod inspector_handler;
mod nav_delegate;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly};
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

impl Engine for WkBackend {
    fn open(&mut self, url: &str) -> EngineResult<PageHandle> {
        let mtm = self.mtm;

        let inspector =
            super::inspector_bridge::InspectorSlots::new(crate::inspector::DEFAULT_BUFFER_CAPACITY);
        let config = unsafe { WKWebViewConfiguration::new(mtm) };
        let ucc = unsafe { config.userContentController() };
        let inspector_installed = inspector_handler::install(mtm, &ucc, &inspector);
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1280.0, 800.0));
        let web_view: Retained<WKWebView> = unsafe {
            WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config)
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
                _nav_delegate: delegate,
                inspector,
                inspector_installed,
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
        super::common::run_save_auth(move |js, budget| eval_js_string(&web_view, js, budget))
    }

    fn load_auth(&mut self, page: PageHandle, blob: &AuthBlob) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_load_auth(
            move |js, budget| eval_js_string(&web_view, js, budget),
            blob,
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
        Ok(p.inspector.details.borrow().get(&seq).cloned())
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
            name: "webkit",
            version: "macOS WebKit (objc2)",
        }
    }
}
