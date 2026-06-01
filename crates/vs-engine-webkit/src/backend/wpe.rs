//! Linux backend — real `WebKitGTK 6` driven via `webkit6` + `glib`.
//!
//! # Threading model
//!
//! WebKitGTK is bound to the GLib main context of the thread that
//! initialized GTK. Methods on [`WpeBackend`] must be called from that
//! thread; the type is `!Send`. Production wiring (follow-up):
//! `vs-cli::serve` on Linux must put GTK on the OS main thread and
//! dispatch engine calls there via `MainContext::invoke`.
//!
//! # Status
//!
//! Real WebKitGTK-backed implementation of every primitive on the
//! [`Engine`] trait: open / close / snapshot / act / wait / layout /
//! set_viewport / save_auth / load_auth — plus inspector capture
//! (console + network) wired through the `UserContentManager` script-
//! message bridge. `capture` is the only outstanding piece (a TODO in
//! a follow-up that calls `WebView::snapshot`).
//!
//! Verified on Linux CI with `libwebkitgtk-6.0-dev` + `libgtk-4-dev`
//! installed; this file is not built on macOS.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use glib::prelude::*;
use webkit6::prelude::*;
use webkit6::{LoadEvent, UserContentInjectedFrames, UserScript, UserScriptInjectionTime, WebView};

use vs_protocol::{Ref, Tree};

use crate::engine::{
    ActTarget, Action, AuthBlob, CaptureScope, Engine, EngineCapabilities, EngineError,
    EngineResult, LayoutBox, PageHandle, Viewport, WaitCondition,
};

// =============================================================================
// Per-page state
// =============================================================================

struct WpePage {
    web_view: WebView,
    inspector: super::inspector_bridge::InspectorSlots,
    /// True if the inspector install path actually succeeded for this
    /// page (script + handlers registered without bail). Read by
    /// `Engine::capabilities()` and the daemon `vs_inspect` gate.
    inspector_installed: bool,
}

// =============================================================================
// Backend
// =============================================================================

/// Real WebKitGTK 6 backend. Construct on the GTK main thread; all
/// subsequent calls must come from the same thread.
pub struct WpeBackend {
    pages: HashMap<PageHandle, WpePage>,
    next_handle: u64,
    captures_dir: Option<PathBuf>,
}

impl Default for WpeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WpeBackend {
    /// Build a backend. Caller must already have initialized GTK on
    /// this thread (via `gtk4::init()`); production wiring lives in
    /// `vs-cli::serve` on Linux. Constructing on a non-GTK thread will
    /// panic the first time a `WebView` operation runs.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pages: HashMap::new(),
            next_handle: 1,
            captures_dir: None,
        }
    }

    /// Pin the directory where `capture` writes PNGs. Defaults to a
    /// system temp subdirectory.
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

    fn page_mut(&mut self, h: PageHandle) -> EngineResult<&mut WpePage> {
        self.pages.get_mut(&h).ok_or(EngineError::NotFound {
            kind: "page",
            id: h.0.to_string(),
        })
    }
}

// =============================================================================
// Run-loop helper: pump the GLib main context until a predicate fires
// =============================================================================

fn run_loop_until<F: FnMut() -> bool>(mut done: F, budget: Duration) -> bool {
    let main_ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if done() {
            return true;
        }
        // Iterate non-blocking; if no work, sleep briefly.
        if !main_ctx.iteration(false) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    done()
}

// =============================================================================
// Shared payloads + parsers
// =============================================================================

use super::common::{parse_snapshot, SNAPSHOT_DOM_WALKER_JS as SNAPSHOT_JS};

// =============================================================================
// Engine impl
// =============================================================================

impl Engine for WpeBackend {
    fn open(&mut self, url: &str) -> EngineResult<PageHandle> {
        let web_view = WebView::new();

        // Pin the User-Agent to a current Safari string so anti-bot
        // fingerprinters don't flag the WebKitGTK default. See the
        // commit log for `crate::engine::DEFAULT_USER_AGENT` rationale.
        if let Some(settings) = WebViewExt::settings(&web_view) {
            settings.set_user_agent(Some(crate::engine::DEFAULT_USER_AGENT));
        }

        // Inspector capture wiring — install the user script + register
        // both message channels BEFORE load_uri so the bridge is live
        // by document-start.
        let inspector =
            super::inspector_bridge::InspectorSlots::new(crate::inspector::DEFAULT_BUFFER_CAPACITY);
        let inspector_installed = install_inspector(&web_view, &inspector);
        web_view.load_uri(url);

        // Wait for LoadEvent::Finished or LoadEvent::Failed via signal.
        let slot: Rc<RefCell<Option<Result<(), String>>>> = Rc::new(RefCell::new(None));
        let slot_for_signal = slot.clone();
        let signal_id = web_view.connect_load_changed(move |_view, event| {
            if event == LoadEvent::Finished {
                *slot_for_signal.borrow_mut() = Some(Ok(()));
            }
        });
        let slot_fail = slot.clone();
        let fail_id = web_view.connect_load_failed(move |_view, _event, _uri, err| {
            *slot_fail.borrow_mut() = Some(Err(err.message().to_string()));
            // Stop event propagation: we have handled it.
            true
        });

        let slot_check = slot.clone();
        let ok = run_loop_until(
            move || slot_check.borrow().is_some(),
            Duration::from_secs(15),
        );

        // Disconnect signal handlers — the WebView outlives this scope.
        web_view.disconnect(signal_id);
        web_view.disconnect(fail_id);

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
            WpePage {
                web_view,
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
        // WebKitGTK doesn't expose a per-WebView viewport API the way
        // WKWebView's setFrame does on macOS. Resize via the GTK widget
        // size request — the WebView is itself a Widget.
        p.web_view.set_size_request(
            i32::try_from(viewport.width).unwrap_or(i32::MAX),
            i32::try_from(viewport.height).unwrap_or(i32::MAX),
        );
        Ok(())
    }

    fn capture(&mut self, page: PageHandle, _scope: CaptureScope) -> EngineResult<PathBuf> {
        let captures_dir = self.captures_dir.clone().unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&captures_dir);
        let path = captures_dir.join(format!("capture-{}.png", page.0));
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        let out_path = path.clone();

        let slot: Rc<RefCell<Option<Result<(), String>>>> = Rc::new(RefCell::new(None));
        let slot_for_cb = slot.clone();
        web_view.snapshot(
            webkit6::SnapshotRegion::FullDocument,
            webkit6::SnapshotOptions::NONE,
            None::<&webkit6::gio::Cancellable>,
            move |result| {
                *slot_for_cb.borrow_mut() = Some(match result {
                    Ok(texture) => texture.save_to_png(&out_path).map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                });
            },
        );

        let slot_check = slot.clone();
        let ok = run_loop_until(
            move || slot_check.borrow().is_some(),
            Duration::from_secs(10),
        );
        if !ok {
            return Err(EngineError::Timeout {
                budget: Duration::from_secs(10),
                primitive: "capture",
            });
        }
        let result = slot.borrow_mut().take();
        match result {
            Some(Ok(())) => Ok(path),
            Some(Err(msg)) => Err(EngineError::Other(format!("capture: {msg}"))),
            None => unreachable!(),
        }
    }

    fn save_auth(&mut self, page: PageHandle) -> EngineResult<AuthBlob> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        // Cookies: host-side via CookieManager so HttpOnly entries
        // are captured. localStorage/sessionStorage: JS shim.
        let cookies = wpe_cookies::get_all_cookies(&web_view)?;
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
        wpe_cookies::set_cookies(&web_view, &parsed.cookies)?;
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
        if matches!(scope, crate::inspector::StorageScope::Cookies) {
            let cookies = wpe_cookies::get_all_cookies(&web_view)?;
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

    fn capabilities(&self) -> EngineCapabilities {
        let any_inspector = self.pages.values().any(|p| p.inspector_installed);
        EngineCapabilities {
            renders: true,
            honors_viewport: true,
            measures_layout: true,
            persists_auth: true,
            inspector_console: any_inspector,
            inspector_network: any_inspector,
            inspector_cookie_events: false,
            name: "wpe",
            version: "Linux WebKitGTK 6 (webkit6)",
        }
    }
}

// =============================================================================
// JS evaluation helper
// =============================================================================

fn eval_js_string(web_view: &WebView, js: &str, budget: Duration) -> EngineResult<String> {
    let slot: Rc<RefCell<Option<Result<String, String>>>> = Rc::new(RefCell::new(None));
    let slot_for_cb = slot.clone();

    let cancellable = webkit6::gio::Cancellable::new();
    web_view.evaluate_javascript(
        js,
        None,
        None,
        Some(&cancellable),
        move |result| match result {
            Ok(value) => {
                let s = value.to_string();
                *slot_for_cb.borrow_mut() = Some(Ok(s));
            }
            Err(err) => {
                *slot_for_cb.borrow_mut() = Some(Err(err.to_string()));
            }
        },
    );

    let slot_check = slot.clone();
    let ok = run_loop_until(move || slot_check.borrow().is_some(), budget);
    if !ok {
        return Err(EngineError::Timeout {
            budget,
            primitive: "eval",
        });
    }
    let result = slot.borrow_mut().take();
    match result {
        Some(Ok(s)) => Ok(s),
        Some(Err(msg)) => Err(EngineError::Other(format!("eval failed: {msg}"))),
        None => unreachable!(),
    }
}

// =============================================================================
// Inspector capture wiring
// =============================================================================

fn install_inspector(web_view: &WebView, slots: &super::inspector_bridge::InspectorSlots) -> bool {
    use super::inspector_bridge::{
        self, InspectorSlots, NetworkIngestSlot, CONSOLE_HANDLER, NETWORK_HANDLER,
    };

    if std::env::var_os("VS_DISABLE_INSPECTOR").is_some() {
        return false;
    }

    let manager = web_view.user_content_manager().expect(
        "WebKitGTK WebView is missing a UserContentManager — should not happen on a default WebView",
    );

    let script = UserScript::new(
        inspector_bridge::SCRIPT,
        UserContentInjectedFrames::AllFrames,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    );
    manager.add_script(&script);

    if !manager.register_script_message_handler(CONSOLE_HANDLER, None) {
        return false;
    }
    if !manager.register_script_message_handler(NETWORK_HANDLER, None) {
        return false;
    }

    let console_slots: InspectorSlots = slots.clone();
    manager.connect_script_message_received(Some(CONSOLE_HANDLER), move |_, value| {
        let json = value.to_string();
        let mut buf = console_slots.console.borrow_mut();
        inspector_bridge::ingest_console(&mut buf, &json);
    });
    let network_slots: InspectorSlots = slots.clone();
    manager.connect_script_message_received(Some(NETWORK_HANDLER), move |_, value| {
        let json = value.to_string();
        let mut entries = network_slots.network.borrow_mut();
        let mut details = network_slots.details.borrow_mut();
        let mut pending = network_slots.pending.borrow_mut();
        inspector_bridge::ingest_network(
            NetworkIngestSlot {
                entries: &mut entries,
                details: &mut details,
                pending: &mut pending,
            },
            &json,
        );
    });
    true
}

// =============================================================================
// Host-side cookie save/load via WebKitCookieManager
// =============================================================================
//
// Mirror of the macOS `webkit::cookie_store` module. `document.cookie`
// can't see or write `HttpOnly` cookies; `WebKitCookieManager` can.
// The v0.1.2 fix routes auth save/load through this path on every
// platform.
mod wpe_cookies {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use webkit6::gio;
    use webkit6::glib;
    use webkit6::prelude::*;
    use webkit6::soup;
    use webkit6::WebView;

    use crate::backend::auth::CookieData;
    use crate::engine::{EngineError, EngineResult};

    use super::run_loop_until;

    const ASYNC_BUDGET: Duration = Duration::from_secs(5);

    fn cookie_manager(web_view: &WebView) -> EngineResult<webkit6::CookieManager> {
        let session = WebViewExt::network_session(web_view).ok_or_else(|| {
            EngineError::Other("WebView has no NetworkSession; cookies unavailable".into())
        })?;
        session
            .cookie_manager()
            .ok_or_else(|| EngineError::Other("NetworkSession has no CookieManager".into()))
    }

    pub(super) fn get_all_cookies(web_view: &WebView) -> EngineResult<Vec<CookieData>> {
        let manager = cookie_manager(web_view)?;
        let slot: Rc<RefCell<Option<Vec<CookieData>>>> = Rc::new(RefCell::new(None));
        let slot_cb = slot.clone();
        manager.all_cookies(None::<&gio::Cancellable>, move |result| {
            let cookies = match result {
                Ok(v) => v.into_iter().map(|mut c| serialize(&mut c)).collect(),
                Err(_) => Vec::new(),
            };
            *slot_cb.borrow_mut() = Some(cookies);
        });
        let check = slot.clone();
        let ok = run_loop_until(move || check.borrow().is_some(), ASYNC_BUDGET);
        if !ok {
            return Err(EngineError::Timeout {
                budget: ASYNC_BUDGET,
                primitive: "save_auth (all_cookies)",
            });
        }
        let result = slot.borrow_mut().take().unwrap_or_default();
        Ok(result)
    }

    pub(super) fn set_cookies(web_view: &WebView, cookies: &[CookieData]) -> EngineResult<()> {
        let manager = cookie_manager(web_view)?;
        for c in cookies {
            if c.name.is_empty() || c.domain.is_empty() {
                continue;
            }
            let path = if c.path.is_empty() {
                "/"
            } else {
                c.path.as_str()
            };
            // max_age=-1: session cookie (no expiration). We set
            // expires explicitly below if the saved blob carried it.
            let mut cookie = soup::Cookie::new(&c.name, &c.value, &c.domain, path, -1);
            cookie.set_secure(c.secure);
            cookie.set_http_only(c.http_only);
            if let Some(unix) = c.expires_unix {
                if let Ok(dt) = glib::DateTime::from_unix_utc(unix) {
                    cookie.set_expires(&dt);
                }
            }
            if let Some(ss) = c.same_site.as_deref() {
                let policy = match ss {
                    "Strict" => soup::SameSitePolicy::Strict,
                    "None" => soup::SameSitePolicy::None,
                    _ => soup::SameSitePolicy::Lax,
                };
                cookie.set_same_site_policy(policy);
            }

            let done: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
            let done_cb = done.clone();
            manager.add_cookie(&cookie, None::<&gio::Cancellable>, move |_| {
                *done_cb.borrow_mut() = true;
            });
            let check = done.clone();
            let ok = run_loop_until(move || *check.borrow(), ASYNC_BUDGET);
            if !ok {
                return Err(EngineError::Timeout {
                    budget: ASYNC_BUDGET,
                    primitive: "load_auth (add_cookie)",
                });
            }
        }
        Ok(())
    }

    fn serialize(c: &mut soup::Cookie) -> CookieData {
        CookieData {
            name: c.name().map(|s| s.to_string()).unwrap_or_default(),
            value: c.value().map(|s| s.to_string()).unwrap_or_default(),
            domain: c.domain().map(|s| s.to_string()).unwrap_or_default(),
            path: c.path().map(|s| s.to_string()).unwrap_or_default(),
            expires_unix: c.expires().map(|dt| dt.to_unix()),
            secure: c.is_secure(),
            http_only: c.is_http_only(),
            same_site: match c.same_site_policy() {
                soup::SameSitePolicy::Strict => Some("Strict".to_string()),
                soup::SameSitePolicy::None => Some("None".to_string()),
                soup::SameSitePolicy::Lax => Some("Lax".to_string()),
                _ => None,
            },
        }
    }
}
