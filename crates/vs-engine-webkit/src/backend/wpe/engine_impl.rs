//! [`Engine`] for [`WpeBackend`]: each primitive looks up the page and
//! calls into the shared logic in [`crate::backend::common`] or the
//! WebKitGTK helpers in the sibling modules.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use glib::prelude::*;
use gtk4 as gtk;
use webkit6::prelude::*;
use webkit6::{LoadEvent, UserContentInjectedFrames, UserScript, UserScriptInjectionTime, WebView};

use vs_protocol::{Ref, Tree};

use super::cookies;
use super::host::{eval_js_string, host_page};
use super::input::{
    click_with, cursor_move_along_path, cursor_press_release, deliver_input, drag_from,
    input_origin, type_keys, wheel, window_title,
};
use super::loading::{load_result, wait_for_load};
use super::{run_loop_until, Opening, WpeBackend};
use crate::engine::{
    ActTarget, Action, AuthBlob, CaptureScope, CursorOp, DataStore, Download, DownloadEntry,
    DownloadSource, Engine, EngineCapabilities, EngineError, EngineResult, InputMode, LayoutBox,
    OpenStart, PageHandle, Viewport, WaitCondition,
};

use crate::backend::common::{parse_snapshot, SNAPSHOT_DOM_WALKER_JS as SNAPSHOT_JS};

impl Engine for WpeBackend {
    fn open(&mut self, url: &str, store: &DataStore) -> EngineResult<PageHandle> {
        let ticket = match self.open_start(url, store)? {
            OpenStart::Done(page) => return Ok(page),
            OpenStart::Pending(ticket) => ticket,
        };
        if let Some(slot) = self.opening.get(&ticket).map(|o| o.slot.clone()) {
            let budget = crate::backend::common::nav_budget();
            let _ = run_loop_until(move || slot.borrow().is_some(), budget);
        }
        loop {
            if let Some(result) = self.open_poll(ticket) {
                return result;
            }
            let _ = run_loop_until(|| false, Duration::from_millis(5));
        }
    }

    fn open_start(&mut self, url: &str, store: &DataStore) -> EngineResult<OpenStart> {
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
        let page = host_page(&web_view, &self.popups);
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

        // The load runs in the page's web process (a new one takes tens
        // of milliseconds to start); `open_poll` finishes the open, and
        // the engine thread serves other pages meanwhile.
        let budget = crate::backend::common::nav_budget();
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        self.opening.insert(
            ticket,
            Opening {
                page,
                slot,
                signals: vec![signal_id, fail_id],
                deadline: std::time::Instant::now() + budget,
                budget,
            },
        );
        Ok(OpenStart::Pending(ticket))
    }

    fn open_poll(&mut self, ticket: u64) -> Option<EngineResult<PageHandle>> {
        let Some(o) = self.opening.get(&ticket) else {
            return Some(Err(EngineError::Other("no open is pending".into())));
        };
        if o.slot.borrow().is_none() && std::time::Instant::now() < o.deadline {
            return None;
        }
        let o = self.opening.remove(&ticket)?;
        // Disconnect the handlers: the WebView outlives the open.
        for id in o.signals {
            o.page.web_view.disconnect(id);
        }
        let outcome = o.slot.borrow_mut().take();
        match outcome {
            None => {
                return Some(Err(EngineError::Timeout {
                    budget: o.budget,
                    primitive: "open",
                    detail: "",
                }))
            }
            Some(Err(msg)) => {
                return Some(Err(EngineError::Other(format!("navigation failed: {msg}"))))
            }
            Some(Ok(())) => {}
        }
        let page = o.page;
        let handle = self.alloc_handle();
        page.me.set(Some(handle));
        page.window.set_title(Some(&window_title(handle)));
        self.pages.insert(handle, page);
        self.place(handle);
        Some(Ok(handle))
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

    fn enable_webauthn(&mut self, page: PageHandle) -> EngineResult<()> {
        // Document-start script for later loads, as on macOS, then the
        // document already loaded.
        self.add_init_script(page, crate::backend::common::WEBAUTHN_JS)?;
        let web_view = self.page_mut(page)?.web_view.clone();
        eval_js_string(
            &web_view,
            crate::backend::common::WEBAUTHN_JS,
            Duration::from_secs(5),
        )?;
        Ok(())
    }

    fn close(&mut self, page: PageHandle) -> EngineResult<()> {
        self.tiles.retain(|_, h| *h != page);
        if let Ok(input) = crate::backend::wpe_input::dispatcher() {
            input.drop_page_pointer(&window_title(page));
        }
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
        mode: InputMode,
    ) -> EngineResult<()> {
        self.forget_new_window(page)?;
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        // A click on a ref is a native one (isTrusted), as on macOS,
        // where the input path is available; else the JS path below.
        if let (ActTarget::Ref(r), Action::Click) = (&target, &action) {
            if crate::backend::wpe_input::dispatcher().is_ok() {
                let at = crate::backend::common::ref_click_point(
                    |js, budget| eval_js_string(&web_view, js, budget),
                    *r,
                )?;
                if let Some((x, y)) = at {
                    return self.cursor_op(page, CursorOp::ClickAt { x, y }, mode);
                }
            }
        }
        crate::backend::common::run_act(
            move |js, budget| eval_js_string(&web_view, js, budget),
            &target,
            &action,
        )?;
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
    fn layout(&mut self, page: PageHandle, refs: &[Ref]) -> EngineResult<Vec<LayoutBox>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        crate::backend::common::run_layout(
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
        let native = p.downloads.clone();
        crate::backend::common::run_download_list(
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

    fn crash(&mut self, page: PageHandle) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        p.web_view.terminate_web_process();
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
        let dispatcher = crate::backend::wpe_input::dispatcher()?;
        let (web_view, home) = {
            let p = self.page_mut(page)?;
            (p.web_view.clone(), p.origin.get())
        };
        let title = window_title(page);
        // XDND needs an X11 window; a pure Wayland session has no
        // protocol for an outside drag, so the drop arrives by script.
        let Some(target) = dispatcher.x11_window(&title) else {
            return Err(EngineError::Unsupported {
                engine: "webkitgtk-wayland",
                primitive: "drop",
            });
        };
        let (ox, oy) = input_origin(dispatcher, page, home);
        dispatcher.raise_window(&title);
        if let Some(js) = crate::backend::drop_shim::install_js(data) {
            eval_js_string(&web_view, &js, Duration::from_secs(2))?;
        }
        let accepted = crate::backend::wpe_xdnd::drop_data(target, ox + x, oy + y, data, |d| {
            let _ = run_loop_until(|| false, d);
        });
        let accepted = accepted?;
        self.follow_new_window(page, false)?;
        Ok(accepted)
    }

    fn set_zoom(&mut self, page: PageHandle, zoom: f64) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        p.web_view.set_zoom_level(zoom);
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
            crate::engine::CookieOp::Set(list) => cookies::set_cookies(&web_view, &list)?,
            crate::engine::CookieOp::Clear => cookies::delete_all(&web_view)?,
        }
        cookies::get_all_cookies(&web_view)
    }

    fn save_auth(&mut self, page: PageHandle) -> EngineResult<AuthBlob> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        // Cookies: host-side via CookieManager so HttpOnly entries
        // are captured. localStorage/sessionStorage: JS shim.
        let cookies = cookies::get_all_cookies(&web_view)?;
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
        cookies::set_cookies(&web_view, &parsed.cookies)?;
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

    fn storage(
        &mut self,
        page: PageHandle,
        scope: crate::inspector::StorageScope,
    ) -> EngineResult<Vec<crate::inspector::StorageEntry>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        if matches!(scope, crate::inspector::StorageScope::Cookies) {
            let cookies = cookies::get_all_cookies(&web_view)?;
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
        Ok(self.page_mut(page)?.main_response.borrow().clone())
    }

    fn cookie_events(
        &mut self,
        page: PageHandle,
    ) -> EngineResult<Vec<crate::inspector::CookieEvent>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        let current = cookies::get_all_cookies(&web_view)?;
        let previous = p.cookie_baseline.borrow().clone();
        let mut seq = p.cookie_next_seq.borrow_mut();
        let events = crate::backend::common::diff_cookies(previous.as_deref(), &current, &mut seq);
        *p.cookie_baseline.borrow_mut() = Some(current);
        Ok(events)
    }

    fn type_text(&mut self, page: PageHandle, text: &str, mode: InputMode) -> EngineResult<()> {
        // Addressing the page validates it and keeps the error shape
        // the same as every other page op, even though the keys go to
        // whatever holds focus rather than to a page handle.
        self.forget_new_window(page)?;
        let (cursor, tile) = {
            let p = self.page_mut(page)?;
            (p.last_mouse.get(), p.origin.get())
        };
        type_keys(page, cursor, tile, text, mode)?;
        self.follow_new_window(page, false)
    }

    fn cursor_op(&mut self, page: PageHandle, op: CursorOp, mode: InputMode) -> EngineResult<()> {
        self.forget_new_window(page)?;
        let p = self.page_mut(page)?;
        let dispatcher = crate::backend::wpe_input::dispatcher()?;
        let humanize_mode = vs_humanize::InputMode::from(mode);
        // The web view fills its window, or is pinned to its top-left once
        // sized (set_viewport), so page coordinates are window coordinates
        // and `input_origin` gives the window's place on the screen.
        let origin = input_origin(dispatcher, page, p.origin.get());
        // Every page's window is at the origin; put this one on top so
        // the input reaches it and not another page's.
        dispatcher.raise_window(&window_title(page));
        dispatcher.use_page_pointer(&window_title(page));
        let start = p.last_mouse.get();
        let seed = crate::backend::common::cursor_seed(op);
        let move_to = |from, to, held| {
            cursor_move_along_path(dispatcher, from, to, origin, humanize_mode, seed, held)
        };
        let point = |x, y| vs_humanize::Point { x, y };
        let landed = match op {
            CursorOp::MoveTo { x, y } | CursorOp::HoverAt { x, y } => {
                move_to(start, point(x, y), false)?
            }
            CursorOp::ClickAt { x, y } => {
                let landed = move_to(start, point(x, y), false)?;
                cursor_press_release(dispatcher, landed, origin, humanize_mode)?;
                landed
            }
            CursorOp::Drag { x1, y1, x2, y2 } => {
                let pressed_at = move_to(start, point(x1, y1), false)?;
                let landed = drag_from(dispatcher, || move_to(pressed_at, point(x2, y2), true))?;
                // After the OS-level drag, fire the HTML5 DragEvent
                // chain in JS so react-dnd / React-Flow HTML5 targets
                // observe the drop.
                let html5_js = crate::backend::common::build_html5_drag_js(x1, y1, x2, y2);
                let _ = eval_js_string(&p.web_view, &html5_js, Duration::from_secs(2));
                landed
            }
            CursorOp::Click { x, y, opts } => {
                let landed = move_to(start, point(x, y), false)?;
                click_with(dispatcher, opts)?;
                landed
            }
            CursorOp::Wheel { x, y, dx, dy } => {
                let landed = move_to(start, point(x, y), false)?;
                wheel(dispatcher, &p.web_view, &p.window, dx, dy)?;
                landed
            }
        };
        p.last_mouse.set(landed);
        deliver_input(dispatcher, &p.window);
        self.follow_new_window(
            page,
            matches!(op, CursorOp::ClickAt { .. } | CursorOp::Click { .. }),
        )
    }

    fn set_popups(&mut self, page: PageHandle, on: bool) -> EngineResult<()> {
        self.page_mut(page)?.popups_on.set(on);
        Ok(())
    }

    fn eval_js_in_frame(
        &mut self,
        page: PageHandle,
        frame: &str,
        expr: &str,
    ) -> EngineResult<crate::inspector::EvalResult> {
        let agents = self
            .page_mut(page)?
            .frames
            .clone()
            .ok_or(EngineError::Unsupported {
                engine: "wpe",
                primitive: "vs_inspect eval --frame",
            })?;
        let not_found = || EngineError::NotFound {
            kind: "frame",
            id: frame.to_string(),
        };
        // A frame still loading has no agent yet; give it a moment.
        let probe = agents.clone();
        let wanted = frame.to_string();
        if !run_loop_until(
            move || crate::backend::wpe_frames::has_agent(&probe, &wanted),
            Duration::from_millis(500),
        ) {
            return Err(not_found());
        }
        crate::backend::common::run_eval(
            |js, budget| {
                let id = crate::backend::wpe_frames::dispatch(&agents, frame, js)
                    .ok_or_else(not_found)?;
                let out: Rc<RefCell<Option<Result<String, String>>>> = Rc::new(RefCell::new(None));
                let (sink, src) = (out.clone(), agents.clone());
                let done = run_loop_until(
                    move || {
                        if let Some(r) = crate::backend::wpe_frames::take_result(&src, id) {
                            *sink.borrow_mut() = Some(r);
                        }
                        sink.borrow().is_some()
                    },
                    budget,
                );
                if !done {
                    return Err(EngineError::Timeout {
                        budget,
                        primitive: "eval",
                        detail: "",
                    });
                }
                let result = out.borrow_mut().take();
                match result {
                    Some(Ok(v)) => Ok(v),
                    Some(Err(e)) => Err(EngineError::Other(e)),
                    None => Err(not_found()),
                }
            },
            expr,
        )
    }

    fn popup_feed(&mut self) -> Option<std::sync::Arc<crate::engine::PopupFeed>> {
        Some(self.popups.borrow().feed.clone())
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
