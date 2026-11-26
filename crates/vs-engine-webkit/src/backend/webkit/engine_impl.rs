//! [`Engine`] for [`WkBackend`]: each primitive looks up the page and
//! calls into the shared logic in [`crate::backend::common`] or the
//! WebKit-specific submodules.

use std::path::PathBuf;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::MainThreadOnly;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSURLRequest, NSURL};
use objc2_web_kit::{WKUserScript, WKUserScriptInjectionTime, WKWebView, WKWebViewConfiguration};
use vs_protocol::{Ref, Tree};

use crate::engine::{
    ActTarget, Action, AuthBlob, CaptureScope, CursorOp, DataStore, Download, DownloadEntry,
    DownloadSource, Engine, EngineCapabilities, EngineError, EngineResult, InputMode, LayoutBox,
    PageHandle, Viewport, WaitCondition,
};

use super::eval::{eval_js_string, run_loop_until};
use super::nav_delegate::wait_for_navigation;
use super::{
    abandon_view, abandon_view_keep_ucc, capture, cookie_store, drop, enable_media_devices, eval,
    frames, host_view, input, inspector_handler, loopback, record, vs_humanize_seed_for_ref,
    vs_humanize_seed_for_xy, WkBackend, WkPage, NO_NATIVE_DRAG_JS, RAF_SHIM_JS,
};

use crate::backend::common::{parse_snapshot, SNAPSHOT_DOM_WALKER_JS as SNAPSHOT_JS, WEBAUTHN_JS};

impl Engine for WkBackend {
    fn open(&mut self, url: &str, store: &DataStore) -> EngineResult<PageHandle> {
        let mtm = self.mtm;

        let inspector = crate::backend::inspector_bridge::InspectorSlots::new(
            crate::inspector::DEFAULT_BUFFER_CAPACITY,
        );
        let config = unsafe { WKWebViewConfiguration::new(mtm) };
        let data_store = self.data_store(store)?;
        unsafe { config.setWebsiteDataStore(&data_store) };
        let proxy_port = store.proxy_port();
        let loopback = proxy_port.is_some()
            && self.wants_loopback(url, Retained::as_ptr(&data_store) as usize);
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
            let src = NSString::from_str(crate::backend::common::DOWNLOAD_SHIM_JS);
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
        // The configuration before any loopback handler: a rebuilt view
        // starts from a copy of it (see `rebuild_view`).
        let base_config: Retained<WKWebViewConfiguration> =
            unsafe { objc2::msg_send![&*config, copy] };
        if let (true, Some(port)) = (loopback, proxy_port) {
            loopback::install(mtm, &config, port);
        }
        let (web_view, window, delegate) =
            host_view(mtm, &config, size, store.proxy_ca().map(<[u8]>::to_vec));
        let slot = delegate.slot();

        let ns_url_str = NSString::from_str(url);
        let ns_url = NSURL::URLWithString(&ns_url_str)
            .ok_or_else(|| EngineError::Other(format!("invalid url: {url}")))?;
        let request = NSURLRequest::requestWithURL(&ns_url);
        let _ = unsafe { web_view.loadRequest(&request) };

        let budget = crate::backend::common::nav_budget();
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
                base_config,
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
        wait_for_navigation(&slot, crate::backend::common::nav_budget(), "navigate")
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
        wait_for_navigation(&slot, crate::backend::common::nav_budget(), "load_html")
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

        let result = crate::backend::common::run_act(
            move |js, budget| eval_js_string(&web_view, js, budget),
            &target,
            &action,
        );
        flush(&web_view_flush);
        result?;
        self.follow_new_window(page, crate::backend::common::may_open_window(&action))
    }

    fn wait(
        &mut self,
        page: PageHandle,
        cond: WaitCondition,
        budget: Duration,
    ) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        crate::backend::common::run_wait(
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
        crate::backend::common::run_download(
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
        crate::backend::common::run_download_list(
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
        crate::backend::common::run_layout(
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

    fn crash(&mut self, page: PageHandle) -> EngineResult<()> {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let p = self.page_mut(page)?;
        // SIGKILL to the web content process: what a real crash looks
        // like to WebKit (asking it to stop the process reports no crash).
        let pid: i32 = unsafe { objc2::msg_send![&*p.web_view, _webProcessIdentifier] };
        if pid <= 0 {
            return Err(EngineError::Other("no web content process".into()));
        }
        unsafe { kill(pid, 9) };
        let _ = run_loop_until(|| false, Duration::from_millis(200));
        Ok(())
    }

    fn drop_data(
        &mut self,
        page: PageHandle,
        x: f64,
        y: f64,
        data: &crate::engine::DropData,
    ) -> EngineResult<bool> {
        self.forget_new_window(page)?;
        let web_view = self.page_mut(page)?.web_view.clone();
        let height = web_view.frame().size.height;
        if let Some(js) = crate::backend::drop_shim::install_js(data) {
            eval_js_string(&web_view, &js, Duration::from_secs(2))?;
        }
        let accepted = drop::drop_data(&web_view, height, x, y, data);
        let _ = eval_js_string(
            &web_view,
            "String(window.__vsFlushRAF ? window.__vsFlushRAF() : 0)",
            Duration::from_secs(2),
        );
        let accepted = accepted?;
        self.follow_new_window(page, false)?;
        Ok(accepted)
    }

    fn set_zoom(&mut self, page: PageHandle, zoom: f64) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        let _: () = unsafe { objc2::msg_send![&*p.web_view, setPageZoom: zoom] };
        Ok(())
    }

    fn cookies(
        &mut self,
        page: PageHandle,
        op: crate::engine::CookieOp,
    ) -> EngineResult<Vec<crate::engine::CookieData>> {
        let web_view = self.page_mut(page)?.web_view.clone();
        match op {
            crate::engine::CookieOp::List => {}
            crate::engine::CookieOp::Set(list) => cookie_store::set_cookies(&web_view, &list)?,
            crate::engine::CookieOp::Clear => cookie_store::delete_all(&web_view)?,
        }
        cookie_store::get_all_cookies(&web_view)
    }

    fn save_auth(&mut self, page: PageHandle) -> EngineResult<AuthBlob> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        // Cookies: host-side via WKHTTPCookieStore so HttpOnly entries
        // are captured (the v0.1.1 document.cookie path silently
        // dropped them). localStorage/sessionStorage: JS shim, which
        // is the right surface for those.
        let cookies = cookie_store::get_all_cookies(&web_view)?;
        let storage = crate::backend::common::run_save_storage_only(
            move |js, budget| eval_js_string(&web_view, js, budget),
            || {
                let _ = run_loop_until(|| false, Duration::from_millis(50));
            },
        )?;
        let blob = crate::backend::auth::AuthBlobV2 {
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
        crate::backend::auth::encode(&blob)
    }

    fn load_auth(&mut self, page: PageHandle, blob: &AuthBlob) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        let parsed = crate::backend::auth::decode(blob)?;
        // Cookies first so subsequent fetches by the storage-restore
        // JS (none today, but future hooks) see the auth state.
        cookie_store::set_cookies(&web_view, &parsed.cookies)?;
        crate::backend::common::run_load_storage_only(
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
        crate::backend::common::run_eval(
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
        crate::backend::common::run_eval(
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
                .map(crate::backend::common::cookie_to_storage_entry)
                .collect());
        }
        crate::backend::common::run_storage(
            move |js, budget| eval_js_string(&web_view, js, budget),
            scope,
        )
    }

    fn scripts(&mut self, page: PageHandle) -> EngineResult<Vec<crate::inspector::ScriptEntry>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        crate::backend::common::run_scripts(move |js, budget| eval_js_string(&web_view, js, budget))
    }

    fn script_source(
        &mut self,
        page: PageHandle,
        seq: u64,
    ) -> EngineResult<Option<crate::inspector::ScriptSource>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        crate::backend::common::run_script_source(
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
        crate::backend::common::run_dom(
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
        crate::backend::common::run_performance(move |js, budget| {
            eval_js_string(&web_view, js, budget)
        })
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
        let events = crate::backend::common::diff_cookies(previous.as_deref(), &current, &mut seq);
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
                let html5_js = crate::backend::common::build_html5_drag_js(x1, y1, x2, y2);
                let _ = eval_js_string(&web_view, &html5_js, std::time::Duration::from_secs(2));
                landed
            }
            CursorOp::Click { x, y, opts } => input::click_with(
                &web_view,
                &window,
                webview_height,
                start,
                vs_humanize::Point { x, y },
                humanize_mode,
                seed,
                rec,
                opts,
            )?,
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
        self.follow_new_window(
            page,
            matches!(op, CursorOp::ClickAt { .. } | CursorOp::Click { .. }),
        )
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
