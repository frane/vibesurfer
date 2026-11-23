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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use glib::prelude::*;
// `gtk4` is a direct dep so we can enable its `v4_10` feature (webkit6
// needs `gtk::Accessible`); alias it to `gtk` to keep the existing refs.
use gtk4 as gtk;
use webkit6::prelude::*;
use webkit6::{LoadEvent, UserContentInjectedFrames, UserScript, UserScriptInjectionTime, WebView};

use vs_protocol::{Ref, Tree};

use crate::engine::{
    ActTarget, Action, AuthBlob, CaptureScope, CursorOp, DataStore, Download, DownloadEntry,
    DownloadSource, Engine, EngineCapabilities, EngineError, EngineResult, InputMode, LayoutBox,
    PageHandle, Viewport, WaitCondition,
};

// =============================================================================
// Per-page state
// =============================================================================

struct WpePage {
    web_view: WebView,
    /// Hidden host window that owns `web_view` as its single child.
    /// Required so the WebView has a `GdkSurface` on the X display —
    /// without it XTest's synthetic input has no window to land on.
    /// Created in `open()`, dropped on `close()`.
    window: gtk::Window,
    inspector: super::inspector_bridge::InspectorSlots,
    /// True if the inspector install path actually succeeded for this
    /// page (script + handlers registered without bail). Read by
    /// `Engine::capabilities()` and the daemon `vs_inspect` gate.
    inspector_installed: bool,
    cookie_baseline: std::cell::RefCell<Option<Vec<super::auth::CookieData>>>,
    cookie_next_seq: std::cell::RefCell<u64>,
    /// Last known cursor position in screen-absolute CSS px. Updated
    /// after every `cursor_op` so the next humanized lead-in starts
    /// where the previous one ended. Defaults to (0, 0) on `open`.
    last_mouse: Cell<vs_humanize::Point>,
    /// A new-window navigation (`target="_blank"`, `window.open()`)
    /// the page asked for and we have not loaded yet. Set by the
    /// `create` handler installed in `open`.
    new_window: Rc<RefCell<Option<webkit6::URIRequest>>>,
    /// Downloads this page's responses turned into.
    downloads: SharedDownloads,
    /// The shared network session's `download-started` handler for
    /// this page. Disconnected on close.
    download_hook: Option<(webkit6::NetworkSession, glib::SignalHandlerId)>,
    /// HTTP response of the latest main-frame navigation. Kept by the
    /// `load-changed` handler from `install_response_capture`.
    main_response: Rc<RefCell<Option<crate::inspector::MainResponse>>>,
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
    /// Ephemeral network sessions for isolated sessions, by key. One
    /// per key so every page of the session shares cookies and storage.
    isolated_sessions: HashMap<String, webkit6::NetworkSession>,
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
            isolated_sessions: HashMap::new(),
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
use super::native_download::{
    is_attachment, name_from_disposition, temp_path, SharedDownloads, MAX_BYTES,
};

// =============================================================================
// Engine impl
// =============================================================================

impl Engine for WpeBackend {
    fn open(&mut self, url: &str, store: &DataStore) -> EngineResult<PageHandle> {
        // An isolated session's pages share one ephemeral (in-memory)
        // network session; everyone else gets the default persistent
        // one. The download hook below connects to whichever session
        // the view ends up with.
        //
        // A proxied session's network session also sends every request
        // through the daemon's proxy. The ignore list is empty, so
        // loopback is proxied too.
        let web_view = match store.key() {
            None => WebView::new(),
            Some(key) => {
                let port = store.proxy_port();
                let session = self
                    .isolated_sessions
                    .entry(key.to_string())
                    .or_insert_with(|| {
                        let s = webkit6::NetworkSession::new_ephemeral();
                        if let Some(port) = port {
                            let uri = format!("http://127.0.0.1:{port}");
                            let settings = webkit6::NetworkProxySettings::new(Some(&uri), &[]);
                            s.set_proxy_settings(
                                webkit6::NetworkProxyMode::Custom,
                                Some(&settings),
                            );
                            // The proxy answers TLS with leaves from the
                            // daemon's CA, made per host, so they cannot
                            // be allowed one by one up front. Every TLS
                            // connection of this session ends at the
                            // proxy, which verifies upstream itself, so
                            // certificate errors are ignored here, for
                            // this network session only.
                            s.set_tls_errors_policy(webkit6::TLSErrorsPolicy::Ignore);
                        }
                        s
                    });
                WebView::builder().network_session(session).build()
            }
        };

        // Hidden host window for the WebView. v0.1.11 added native
        // input dispatch (XTest / libei) for the cursor primitives;
        // both require the WebView to have a real `GdkSurface` on the
        // display server so the synthetic event has somewhere to land.
        // `WebView::new()` alone renders to an internal offscreen
        // surface with no display-server window — XTest events would
        // disappear into the void. Wrapping each WebView in a
        // decorated-off `gtk::Window` and presenting it gives us that
        // surface; under `xvfb` (CI) the window has no visual effect.
        let window = gtk::Window::new();
        window.set_decorated(false);
        window.set_default_size(1280, 800);
        window.set_child(Some(&web_view));
        window.present();

        // Pin the User-Agent to a current Safari string so anti-bot
        // fingerprinters don't flag the WebKitGTK default. See the
        // commit log for `crate::engine::DEFAULT_USER_AGENT` rationale.
        if let Some(settings) = WebViewExt::settings(&web_view) {
            settings.set_user_agent(Some(crate::engine::DEFAULT_USER_AGENT));
            // Expose `navigator.mediaDevices`. WebKitGTK gates the
            // MediaDevices surface off by default, so the object is
            // simply absent and anything feature-detecting camera or
            // mic support sees a browser that cannot exist. The macOS
            // backend needed the same switch (there via a WKPreferences
            // KVC key); here it is public API.
            settings.set_enable_media_stream(true);
        }

        // Inspector capture wiring — install the user script + register
        // both message channels BEFORE load_uri so the bridge is live
        // by document-start.
        let inspector =
            super::inspector_bridge::InspectorSlots::new(crate::inspector::DEFAULT_BUFFER_CAPACITY);
        let inspector_installed = install_inspector(&web_view, &inspector);
        install_download_shim(&web_view);
        // A headless page has no second window. Without a `create`
        // handler WebKitGTK drops a `target="_blank"` link or form, or
        // `window.open()`, and the click does nothing. Park the request;
        // the input primitive that caused it loads it in this page
        // (`follow_new_window`). `act click` on this engine is a JS
        // click, not a user gesture, so blank-target navigation also
        // has to be allowed without one.
        if let Some(settings) = WebViewExt::settings(&web_view) {
            settings.set_javascript_can_open_windows_automatically(true);
        }
        let new_window: Rc<RefCell<Option<webkit6::URIRequest>>> = Rc::new(RefCell::new(None));
        let parked = new_window.clone();
        web_view.connect_create(move |_view, action| {
            *parked.borrow_mut() = action.request();
            None
        });
        let downloads = SharedDownloads::default();
        let download_hook = install_download_capture(&web_view, &downloads);
        let main_response = install_response_capture(&web_view);
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
            *slot_fail.borrow_mut() = Some(load_result(err));
            // Stop event propagation: we have handled it.
            true
        });

        let slot_check = slot.clone();
        let budget = super::common::nav_budget();
        let ok = run_loop_until(move || slot_check.borrow().is_some(), budget);

        // Disconnect signal handlers — the WebView outlives this scope.
        web_view.disconnect(signal_id);
        web_view.disconnect(fail_id);

        if !ok {
            return Err(EngineError::Timeout {
                budget,
                primitive: "open",
                detail: "",
            });
        }
        match slot.borrow_mut().take() {
            Some(Ok(())) => {}
            Some(Err(msg)) => return Err(EngineError::Other(format!("navigation failed: {msg}"))),
            None => {
                return Err(EngineError::Other(
                    "navigation completed without a result".into(),
                ))
            }
        }

        let handle = self.alloc_handle();
        self.pages.insert(
            handle,
            WpePage {
                web_view,
                window: titled(window, handle),
                inspector,
                inspector_installed,
                cookie_baseline: std::cell::RefCell::new(None),
                cookie_next_seq: std::cell::RefCell::new(0),
                last_mouse: Cell::new(vs_humanize::Point { x: 0.0, y: 0.0 }),
                new_window,
                downloads,
                download_hook,
                main_response,
            },
        );
        Ok(handle)
    }

    fn navigate(&mut self, page: PageHandle, url: &str) -> EngineResult<()> {
        // Same wait as `open`, on a WebView that already exists. The
        // inspector bridge and download shim were installed at
        // document-start and survive the navigation, so this only has
        // to load and wait.
        let web_view = self.page_mut(page)?.web_view.clone();
        web_view.load_uri(url);
        wait_for_load(&web_view, "navigate")
    }

    fn load_html(&mut self, page: PageHandle, url: &str, html: &str) -> EngineResult<()> {
        let web_view = self.page_mut(page)?.web_view.clone();
        web_view.load_html(html, Some(url));
        wait_for_load(&web_view, "load_html")
    }

    fn add_init_script(&mut self, page: PageHandle, source: &str) -> EngineResult<()> {
        let manager = self
            .page_mut(page)?
            .web_view
            .user_content_manager()
            .ok_or_else(|| EngineError::Other("WebView has no UserContentManager".into()))?;
        manager.add_script(&UserScript::new(
            source,
            UserContentInjectedFrames::AllFrames,
            UserScriptInjectionTime::Start,
            &[],
            &[],
        ));
        Ok(())
    }

    fn enable_webauthn(&mut self, _page: PageHandle) -> EngineResult<()> {
        Err(EngineError::NotImplemented {
            engine: "wpe",
            primitive: "enable_webauthn",
        })
    }

    fn close(&mut self, page: PageHandle) -> EngineResult<()> {
        if let Some(mut p) = self.pages.remove(&page) {
            if let Some((session, id)) = p.download_hook.take() {
                session.disconnect(id);
            }
            // Dismiss the hidden host window so its `GdkSurface` is
            // unmapped from the display server. `close()` queues the
            // destroy; the GLib main context completes it on the next
            // iteration.
            p.window.close();
        }
        Ok(())
    }

    fn drop_store(&mut self, key: &str) -> EngineResult<()> {
        // The session lives on while a closed view still references it;
        // dropping ours lets GLib free it with the last one.
        self.isolated_sessions.remove(key);
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
        _mode: InputMode,
    ) -> EngineResult<()> {
        self.forget_new_window(page)?;
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_act(
            move |js, budget| eval_js_string(&web_view, js, budget),
            &target,
            &action,
        )?;
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
    fn layout(&mut self, page: PageHandle, refs: &[Ref]) -> EngineResult<Vec<LayoutBox>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_layout(
            move |js, budget| eval_js_string(&web_view, js, budget),
            refs,
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
        let native = p.downloads.clone();
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
        let native = p.downloads.clone();
        super::common::run_download_list(
            move |js, budget| eval_js_string(&web_view, js, budget),
            &native,
        )
    }
    fn set_viewport(&mut self, page: PageHandle, viewport: Viewport) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        let w = i32::try_from(viewport.width).unwrap_or(i32::MAX);
        let h = i32::try_from(viewport.height).unwrap_or(i32::MAX);
        // The host `gtk::Window` constrains its child WebView's
        // allocation, so resizing the WebView via `set_size_request`
        // alone is shadowed by the (1280x800) host. Resize both: the
        // window's default size for the surface, and the WebView's
        // size request so its render target matches the requested
        // viewport.
        p.window.set_default_size(w, h);
        p.web_view.set_size_request(w, h);
        // Pin the sized view to the window's top-left corner: a viewport
        // smaller than the window must not be centered, or pointer input
        // at page coordinates would miss by the gap. (Without a size
        // request the view fills the window, so this is only set here.)
        p.web_view.set_halign(gtk::Align::Start);
        p.web_view.set_valign(gtk::Align::Start);
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
                detail: "",
            });
        }
        let result = slot.borrow_mut().take();
        match result {
            Some(Ok(())) => Ok(path),
            Some(Err(msg)) => Err(EngineError::Other(format!("capture: {msg}"))),
            None => Err(EngineError::Other(
                "capture completed without a result".into(),
            )),
        }
    }

    fn save_auth(&mut self, page: PageHandle) -> EngineResult<AuthBlob> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        // Cookies: host-side via CookieManager so HttpOnly entries
        // are captured. localStorage/sessionStorage: JS shim.
        let cookies = wpe_cookies::get_all_cookies(&web_view)?;
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
        wpe_cookies::set_cookies(&web_view, &parsed.cookies)?;
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

    fn main_response(
        &mut self,
        page: PageHandle,
    ) -> EngineResult<Option<crate::inspector::MainResponse>> {
        Ok(self.page_mut(page)?.main_response.borrow().clone())
    }

    fn cookie_events(
        &mut self,
        page: PageHandle,
    ) -> EngineResult<Vec<crate::inspector::CookieEvent>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        let current = wpe_cookies::get_all_cookies(&web_view)?;
        let previous = p.cookie_baseline.borrow().clone();
        let mut seq = p.cookie_next_seq.borrow_mut();
        let events = super::common::diff_cookies(previous.as_deref(), &current, &mut seq);
        *p.cookie_baseline.borrow_mut() = Some(current);
        Ok(events)
    }

    fn type_text(&mut self, page: PageHandle, text: &str, mode: InputMode) -> EngineResult<()> {
        // Addressing the page validates it and keeps the error shape
        // the same as every other page op, even though the keys go to
        // whatever holds focus rather than to a page handle.
        self.forget_new_window(page)?;
        let cursor = self.page_mut(page)?.last_mouse.get();
        let dispatcher = super::wpe_input::dispatcher()?;
        // Pump GLib rather than sleep: this runs on the main thread,
        // so a plain sleep would stop WebKit processing the very keys
        // we are sending.
        let settle = |d: Duration| {
            let _ = run_loop_until(|| false, d);
        };
        // Put the pointer where we believe it already is before asking
        // for focus. X hands keys to the focused window, and a
        // headless session has no window manager to focus anything, so
        // the pointer is what we have to go on. The caller has
        // normally just clicked to place the caret, which means the
        // right window is already under it.
        #[allow(clippy::cast_possible_truncation)]
        let at = super::wpe_input::ScreenPoint {
            x: cursor.x as i32,
            y: cursor.y as i32,
        };
        dispatcher.raise_window(&window_title(page));
        let _ = dispatcher.dispatch(super::wpe_input::InputEvent::Move(at));
        let _ = dispatcher.flush();
        dispatcher.focus_pointer_window();

        let (base_delay, span) = match mode {
            InputMode::Robotic => (0_u64, 0_u64),
            InputMode::Careful => (120, 0),
            // ±40% of base, so repeated typing is not metronomic.
            InputMode::Human => (45, 18),
        };
        let mut jitter = text
            .chars()
            .map(|c| c as u64)
            .fold(0x9e37_79b9_u64, |a, c| a.wrapping_mul(31).wrapping_add(c))
            | 1;

        let typed = (|| -> EngineResult<()> {
            for ch in text.chars() {
                let hold = if base_delay == 0 { 1 } else { base_delay / 3 };
                dispatcher.key_down(ch, &settle)?;
                settle(Duration::from_millis(hold.max(1)));
                dispatcher.key_up(ch)?;
                if base_delay > 0 {
                    jitter ^= jitter << 13;
                    jitter ^= jitter >> 7;
                    jitter ^= jitter << 17;
                    let extra = if span == 0 {
                        0
                    } else {
                        jitter % (span * 2 + 1)
                    };
                    let wait = base_delay.saturating_sub(span).saturating_add(extra);
                    settle(Duration::from_millis(wait.max(1)));
                } else {
                    settle(Duration::from_millis(1));
                }
            }
            Ok(())
        })();

        // Give the borrowed keycode back whether the run finished or
        // failed halfway: a stray binding would change what the user's
        // own keyboard does for the rest of the session.
        let restored = dispatcher.end_typing();
        typed?;
        restored?;

        // Let the last keyUp turn into beforeinput/input before the
        // caller reads the page back.
        let _ = run_loop_until(|| false, Duration::from_millis(150));
        self.follow_new_window(page, false)
    }

    fn cursor_op(&mut self, page: PageHandle, op: CursorOp, mode: InputMode) -> EngineResult<()> {
        self.forget_new_window(page)?;
        let p = self.page_mut(page)?;
        let dispatcher = super::wpe_input::dispatcher()?;
        let humanize_mode = match mode {
            InputMode::Human => vs_humanize::InputMode::Human,
            InputMode::Careful => vs_humanize::InputMode::Careful,
            InputMode::Robotic => vs_humanize::InputMode::Robotic,
        };
        // Screen origin of the hidden host window. v0.1.11 assumes
        // (0, 0) — true under `xvfb` (CI) and any session without a
        // window manager. With a WM, the WM may have translated the
        // window elsewhere; a follow-up PR will query
        // `GdkX11Surface::xid()` + `XTranslateCoordinates` to read the
        // real origin. Until then, page-local CSS px == screen px,
        // which is fine for CI and most headless setups.
        // The web view fills the window, or is pinned to its top-left
        // once sized (set_viewport): page coordinates are window
        // coordinates either way.
        let (origin_x, origin_y) = (0.0_f64, 0.0_f64);
        // Every page's window is at the origin; put this one on top so
        // the input reaches it and not another page's.
        dispatcher.raise_window(&window_title(page));
        let start = p.last_mouse.get();
        let seed = humanize_seed(op);
        let landed = match op {
            CursorOp::MoveTo { x, y } | CursorOp::HoverAt { x, y } => cursor_move_along_path(
                dispatcher,
                start,
                vs_humanize::Point { x, y },
                (origin_x, origin_y),
                humanize_mode,
                seed,
                false,
            )?,
            CursorOp::ClickAt { x, y } => {
                let landed = cursor_move_along_path(
                    dispatcher,
                    start,
                    vs_humanize::Point { x, y },
                    (origin_x, origin_y),
                    humanize_mode,
                    seed,
                    false,
                )?;
                cursor_press_release(dispatcher, landed, (origin_x, origin_y))?;
                landed
            }
            CursorOp::Drag { x1, y1, x2, y2 } => {
                let start_pt = vs_humanize::Point { x: x1, y: y1 };
                let target = vs_humanize::Point { x: x2, y: y2 };
                let pre = cursor_move_along_path(
                    dispatcher,
                    start,
                    start_pt,
                    (origin_x, origin_y),
                    humanize_mode,
                    seed,
                    false,
                )?;
                dispatcher.dispatch(super::wpe_input::InputEvent::Press(
                    super::wpe_input::Button::Left,
                ))?;
                std::thread::sleep(Duration::from_millis(15));
                let landed = cursor_move_along_path(
                    dispatcher,
                    pre,
                    target,
                    (origin_x, origin_y),
                    humanize_mode,
                    seed,
                    true,
                )?;
                dispatcher.dispatch(super::wpe_input::InputEvent::Release(
                    super::wpe_input::Button::Left,
                ))?;
                dispatcher.flush()?;
                // After the OS-level drag, fire the HTML5 DragEvent
                // chain in JS so react-dnd / React-Flow HTML5 targets
                // observe the drop.
                let html5_js = super::common::build_html5_drag_js(x1, y1, x2, y2);
                let web_view = p.web_view.clone();
                let _ = eval_js_string(&web_view, &html5_js, Duration::from_secs(2));
                landed
            }
        };
        p.last_mouse.set(landed);
        self.follow_new_window(page, matches!(op, CursorOp::ClickAt { .. }))
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
            inspector_cookie_events: true,
            name: "wpe",
            version: "Linux WebKitGTK 6 (webkit6)",
        }
    }
}

// =============================================================================
// JS evaluation helper
// =============================================================================

// =============================================================================
// Cursor primitive helpers (XTest / libei input dispatch)
// =============================================================================

/// Deterministic seed for the humanize Bezier path so the same
/// `cursor_op` invocation produces the same trajectory on every run.
fn humanize_seed(op: CursorOp) -> u64 {
    // Hash the op's coordinates into a u64. Trivial mixer; replays
    // need determinism, not cryptographic spread.
    let (a, b, c, d) = match op {
        CursorOp::MoveTo { x, y } | CursorOp::HoverAt { x, y } | CursorOp::ClickAt { x, y } => {
            (x, y, 0.0, 0.0)
        }
        CursorOp::Drag { x1, y1, x2, y2 } => (x1, y1, x2, y2),
    };
    let bits = |v: f64| v.to_bits();
    bits(a).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ bits(b).wrapping_mul(0xBF58_476D_1CE4_E5B9)
        ^ bits(c).wrapping_mul(0x94D0_49BB_1331_11EB)
        ^ bits(d)
}

/// Walk a Bezier path from `start` to `end`, dispatching one `Move`
/// per step at the path's prescribed timing. Returns the final point.
/// The `button_down` flag is unused on Linux today (XTest doesn't
/// distinguish drag-move from plain-move — both are `MotionNotify`
/// events; the held button state is tracked at the X server). Kept
/// in the signature so drag callers stay symmetric with the macOS
/// `move_along_path`.
/// The X window title of a page's host window. Every host window sits
/// at the screen origin and XTest input goes to whichever is on top, so
/// the input path finds a page's window by this name and raises it.
fn window_title(page: PageHandle) -> String {
    format!("vs-page-{}", page.0)
}

fn titled(window: gtk::Window, page: PageHandle) -> gtk::Window {
    window.set_title(Some(&window_title(page)));
    window
}

fn cursor_move_along_path(
    dispatcher: &dyn super::wpe_input::InputDispatcher,
    start: vs_humanize::Point,
    end: vs_humanize::Point,
    origin: (f64, f64),
    mode: vs_humanize::InputMode,
    seed: u64,
    _button_down: bool,
) -> EngineResult<vs_humanize::Point> {
    let path = vs_humanize::mouse_path(start, end, mode, seed);
    let mut prev_ms: u128 = 0;
    for step in &path {
        if step.kind != vs_humanize::MouseStepKind::Move {
            continue;
        }
        let screen = super::wpe_input::ScreenPoint {
            #[allow(clippy::cast_possible_truncation)]
            x: (origin.0 + step.point.x).round() as i32,
            #[allow(clippy::cast_possible_truncation)]
            y: (origin.1 + step.point.y).round() as i32,
        };
        dispatcher.dispatch(super::wpe_input::InputEvent::Move(screen))?;
        let dt = step.at.as_millis().saturating_sub(prev_ms);
        if dt > 0 {
            std::thread::sleep(Duration::from_millis(u64::try_from(dt).unwrap_or(0)));
        }
        prev_ms = step.at.as_millis();
    }
    // Final settling move ending exactly at `end`.
    let final_pt = super::wpe_input::ScreenPoint {
        #[allow(clippy::cast_possible_truncation)]
        x: (origin.0 + end.x).round() as i32,
        #[allow(clippy::cast_possible_truncation)]
        y: (origin.1 + end.y).round() as i32,
    };
    dispatcher.dispatch(super::wpe_input::InputEvent::Move(final_pt))?;
    dispatcher.flush()?;
    Ok(end)
}

/// Press-then-release left button at the current pointer position.
/// Matches the `mouseDown` / 15 ms / `mouseUp` cadence the macOS
/// backend uses so JS sees a coherent `click` event.
fn cursor_press_release(
    dispatcher: &dyn super::wpe_input::InputDispatcher,
    _at: vs_humanize::Point,
    _origin: (f64, f64),
) -> EngineResult<()> {
    dispatcher.dispatch(super::wpe_input::InputEvent::Press(
        super::wpe_input::Button::Left,
    ))?;
    std::thread::sleep(Duration::from_millis(15));
    dispatcher.dispatch(super::wpe_input::InputEvent::Release(
        super::wpe_input::Button::Left,
    ))?;
    dispatcher.flush()?;
    std::thread::sleep(Duration::from_millis(30));
    Ok(())
}

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
            detail: "",
        });
    }
    let result = slot.borrow_mut().take();
    match result {
        Some(Ok(s)) => Ok(s),
        Some(Err(msg)) => Err(EngineError::Other(format!("eval failed: {msg}"))),
        None => Err(EngineError::Other("eval completed without a result".into())),
    }
}

// =============================================================================
// Inspector capture wiring
// =============================================================================

/// Add the download-capture shim as a document-start script in every
/// frame. WebKitGTK does emit a `download-started` signal, but the
/// shim covers what that signal cannot see — an in-page `blob:` save
/// whose object URL is revoked a tick later — and keeps the capture
/// buffer identical across all three backends.
fn install_download_shim(web_view: &WebView) {
    let Some(manager) = web_view.user_content_manager() else {
        return;
    };
    manager.add_script(&UserScript::new(
        super::common::DOWNLOAD_SHIM_JS,
        UserContentInjectedFrames::AllFrames,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
}

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
                detail: "",
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
                    detail: "",
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

/// A load that ended because its response became a download left the
/// page as it was and put the file in the download buffer. Not a
/// failure.
fn load_result(err: &glib::Error) -> Result<(), String> {
    if err.matches(webkit6::PolicyError::FrameLoadInterruptedByPolicyChange) {
        Ok(())
    } else {
        Err(err.message().to_string())
    }
}

/// Keep the HTTP response of each main-frame navigation.
///
/// `load-changed` fires for the main frame only. The slot is cleared
/// when a load starts, so a failed or non-HTTP load reports nothing,
/// and filled at commit, when the main resource has its response.
fn install_response_capture(
    web_view: &WebView,
) -> Rc<RefCell<Option<crate::inspector::MainResponse>>> {
    let slot: Rc<RefCell<Option<crate::inspector::MainResponse>>> = Rc::new(RefCell::new(None));
    let sink = slot.clone();
    web_view.connect_load_changed(move |view, event| match event {
        LoadEvent::Started => {
            sink.borrow_mut().take();
        }
        LoadEvent::Committed => {
            *sink.borrow_mut() = view
                .main_resource()
                .and_then(|r| r.response())
                .and_then(|r| http_response(&r));
        }
        _ => {}
    });
    slot
}

fn http_response(resp: &webkit6::URIResponse) -> Option<crate::inspector::MainResponse> {
    let url = resp.uri().map(|u| u.to_string()).unwrap_or_default();
    if !(url.starts_with("http:") || url.starts_with("https:")) {
        return None;
    }
    let status = u16::try_from(resp.status_code()).unwrap_or(0);
    if status == 0 {
        return None;
    }
    let mut headers = Vec::new();
    if let Some(h) = resp.http_headers() {
        h.foreach(|name, value| headers.push((name.to_ascii_lowercase(), value.to_string())));
    }
    Some(crate::inspector::MainResponse {
        url,
        status,
        status_text: crate::inspector::reason_phrase(status).to_string(),
        headers,
    })
}

/// Save attachments instead of dropping them.
///
/// A `Content-Disposition: attachment` response (an export button, a
/// form POST answering with a file, a hidden-iframe download) never
/// reaches page script. Turn it into a WebKit download, write it to a
/// temp file, and move the bytes into `downloads` for `vs_download`.
/// The network session is shared by every page of the same store, so
/// the `download-started` handler checks the download belongs to this
/// one.
fn install_download_capture(
    web_view: &WebView,
    downloads: &SharedDownloads,
) -> Option<(webkit6::NetworkSession, glib::SignalHandlerId)> {
    web_view.connect_decide_policy(|_view, decision, ty| {
        if ty != webkit6::PolicyDecisionType::Response {
            return false;
        }
        let Some(rd) = decision.downcast_ref::<webkit6::ResponsePolicyDecision>() else {
            return false;
        };
        let Some(resp) = rd.response() else {
            return false;
        };
        let uri = resp.uri().map(|u| u.to_string()).unwrap_or_default();
        if !(uri.starts_with("http:") || uri.starts_with("https:")) {
            return false;
        }
        let cd = resp
            .http_headers()
            .and_then(|h| h.one("Content-Disposition"))
            .map(|v| v.to_string())
            .unwrap_or_default();
        if is_attachment(&cd) || !rd.is_mime_type_supported() {
            decision.download();
            return true;
        }
        false
    });

    let session = WebViewExt::network_session(web_view)?;
    let weak = web_view.downgrade();
    let downloads = downloads.clone();
    let id = session.connect_download_started(move |_session, download| {
        let Some(view) = weak.upgrade() else { return };
        if download.web_view().as_ref() != Some(&view) {
            return;
        }
        let resp = download.response();
        let url = resp
            .as_ref()
            .and_then(webkit6::URIResponse::uri)
            .or_else(|| download.request().and_then(|r| r.uri()))
            .map(|u| u.to_string())
            .unwrap_or_default();
        let mime = resp
            .as_ref()
            .and_then(webkit6::URIResponse::mime_type)
            .map(|m| m.to_string())
            .unwrap_or_default();
        let name = resp
            .as_ref()
            .and_then(webkit6::URIResponse::http_headers)
            .and_then(|h| h.one("Content-Disposition"))
            .and_then(|cd| name_from_disposition(&cd))
            .unwrap_or_default();
        let Ok(id) = downloads.lock().map(|mut d| d.begin(&url, &name, &mime)) else {
            return;
        };
        let path = temp_path(id);
        let _ = std::fs::remove_file(&path);

        let dl = downloads.clone();
        download.connect_decide_destination(move |download, suggested| {
            if let Ok(mut d) = dl.lock() {
                if name.is_empty() {
                    d.set_name(id, suggested);
                }
            }
            let expected = download.response().map_or(0, |r| r.content_length());
            if expected > MAX_BYTES {
                if let Ok(mut d) = dl.lock() {
                    d.finish(
                        id,
                        Err(format!("download exceeds {MAX_BYTES} bytes ({expected})")),
                    );
                }
                download.cancel();
                return true;
            }
            download.set_destination(&path.to_string_lossy());
            true
        });
        let dl = downloads.clone();
        download.connect_failed(move |_download, err| {
            if let Ok(mut d) = dl.lock() {
                d.finish(id, Err(err.message().to_string()));
            }
            let _ = std::fs::remove_file(temp_path(id));
        });
        let dl = downloads.clone();
        download.connect_finished(move |_download| {
            let path = temp_path(id);
            // `failed` is followed by `finished`; the first result wins.
            let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()));
            let _ = std::fs::remove_file(&path);
            if let Ok(mut d) = dl.lock() {
                d.finish(id, bytes);
            }
        });
    });
    Some((session, id))
}

/// Wait for the load already started on `web_view` to finish or fail.
fn wait_for_load(web_view: &WebView, primitive: &'static str) -> EngineResult<()> {
    let slot: Rc<RefCell<Option<Result<(), String>>>> = Rc::new(RefCell::new(None));
    let slot_for_signal = slot.clone();
    let signal_id = web_view.connect_load_changed(move |_view, event| {
        if event == LoadEvent::Finished {
            *slot_for_signal.borrow_mut() = Some(Ok(()));
        }
    });
    let slot_fail = slot.clone();
    let fail_id = web_view.connect_load_failed(move |_view, _event, _uri, err| {
        *slot_fail.borrow_mut() = Some(load_result(err));
        true
    });

    let slot_check = slot.clone();
    let budget = super::common::nav_budget();
    let ok = run_loop_until(move || slot_check.borrow().is_some(), budget);

    web_view.disconnect(signal_id);
    web_view.disconnect(fail_id);

    if !ok {
        return Err(EngineError::Timeout {
            budget,
            primitive,
            detail: "",
        });
    }
    // Taken out of the RefCell before the match, not inside it:
    // as the function's tail expression the borrow guard would
    // otherwise outlive `slot` itself.
    let result = slot.borrow_mut().take();
    match result {
        Some(Ok(())) => Ok(()),
        Some(Err(msg)) => Err(EngineError::Other(format!("navigation failed: {msg}"))),
        None => Err(EngineError::Other(
            "navigation completed without a result".into(),
        )),
    }
}

impl WpeBackend {
    /// Drop a new-window request parked before this input primitive
    /// started (a popup the page fired on its own), so the primitive
    /// only follows what it caused.
    fn forget_new_window(&mut self, page: PageHandle) -> EngineResult<()> {
        let _ = self.page_mut(page)?.new_window.borrow_mut().take();
        Ok(())
    }

    /// Load a new-window navigation the last input primitive caused, in
    /// this page, and wait for it like `goto`. The `create` signal can
    /// arrive after the click returns, so after a click (`settle`) pump
    /// for up to `NEW_WINDOW_SETTLE` first. No parked request: no-op.
    fn follow_new_window(&mut self, page: PageHandle, settle: bool) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        if settle && p.new_window.borrow().is_none() {
            let hint = eval_js_string(
                &p.web_view,
                super::common::NEW_WINDOW_HINT_JS,
                Duration::from_secs(2),
            );
            if super::common::new_window_hinted(&hint) {
                let parked = p.new_window.clone();
                let _ = run_loop_until(
                    move || parked.borrow().is_some(),
                    super::common::NEW_WINDOW_SETTLE,
                );
            }
        }
        let Some(request) = p.new_window.borrow_mut().take() else {
            return Ok(());
        };
        let web_view = p.web_view.clone();
        web_view.load_request(&request);
        wait_for_load(&web_view, "act")
    }
}
