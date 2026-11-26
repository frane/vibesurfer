//! [`Engine`] for [`Webview2Backend`]: each primitive looks up the page
//! and calls into the shared logic in [`crate::backend::common`] or the
//! WebView2 helpers in the sibling modules.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use vs_protocol::{Ref, Tree};

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2CompositionController, ICoreWebView2Environment, ICoreWebView2Environment3,
    COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN,
    COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP,
};
use webview2_com::{
    pwstr_from_str, CallDevToolsProtocolMethodCompletedHandler, CapturePreviewCompletedHandler,
    CreateCoreWebView2CompositionControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, NavigationCompletedEventHandler,
};
use windows::core::{Interface, HSTRING};
use windows::Win32::Foundation::{E_POINTER, RECT};

use crate::backend::common::SNAPSHOT_DOM_WALKER_JS as SNAPSHOT_JS;
use crate::engine::{
    ActTarget, Action, AuthBlob, CaptureScope, CursorOp, DataStore, Download, DownloadEntry,
    DownloadSource, Engine, EngineCapabilities, EngineError, EngineResult, InputMode, LayoutBox,
    PageHandle, Viewport, WaitCondition,
};

use super::cookies;
use super::host::{host_view, isolated_controller_options, proxied_user_data_dir};
use super::input::{
    point_at, wv2_humanize_seed, wv2_move_along_path, wv2_press_release, wv2_send_mouse, wv2_wheel,
};
use super::script::{add_init_script, call_cdp, cdp_click, cdp_key, execute_script};
use super::{ViewOrigin, Webview2Backend};

impl Engine for Webview2Backend {
    // The COM bring-up (environment → composition controller →
    // DirectComposition device/target/visual → settings → navigate)
    // is one linear sequence; splitting it would just scatter the
    // handle plumbing.
    #[allow(clippy::too_many_lines)]
    fn open(&mut self, url: &str, store: &DataStore) -> EngineResult<PageHandle> {
        let parent = self.create_host_hwnd()?;

        // 1. Environment. A proxied session needs its own browser
        //    process: proxy flags are browser arguments, and every
        //    environment on one user data folder must pass the same
        //    ones. So it gets a per-session folder under the temp dir.
        //    `<-loopback>` stops Chromium's implicit loopback bypass,
        //    so local test servers are proxied too.
        let proxied = match store {
            DataStore::Proxied {
                key,
                port,
                spki_pins,
                ..
            } => Some((proxied_user_data_dir(key), *port, spki_pins.clone())),
            _ => None,
        };
        let environment: ICoreWebView2Environment = {
            let (tx, rx) = mpsc::channel();
            CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
                Box::new(move |handler| unsafe {
                    match proxied {
                        None => {
                            webview2_com::Microsoft::Web::WebView2::Win32::CreateCoreWebView2Environment(
                                &handler,
                            )
                        }
                        Some((dir, port, pins)) => {
                            // The proxy answers TLS with leaves from the
                            // daemon's CA. Chromium accepts a chain with
                            // any of these public keys despite the
                            // unknown root; the flag applies to this
                            // session's browser process only. Edge's own
                            // background traffic (component updates,
                            // telemetry) would otherwise reach the proxy
                            // and the session's event stream as if the
                            // page had made it.
                            let options = webview2_com::CoreWebView2EnvironmentOptions::default();
                            options.set_additional_browser_arguments(format!(
                                "--proxy-server=http://127.0.0.1:{port} --proxy-bypass-list=<-loopback> --ignore-certificate-errors-spki-list={pins} --disable-background-networking --disable-component-update --disable-domain-reliability"
                            ));
                            let options: webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2EnvironmentOptions =
                                options.into();
                            webview2_com::Microsoft::Web::WebView2::Win32::CreateCoreWebView2EnvironmentWithOptions(
                                windows::core::PCWSTR::null(),
                                &HSTRING::from(dir.as_os_str()),
                                &options,
                                &handler,
                            )
                        }
                    }
                    .map_err(webview2_com::Error::WindowsError)
                }),
                Box::new(move |error_code, environment| {
                    error_code?;
                    tx.send(environment.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                        .map_err(|_| windows::core::Error::from(E_POINTER))?;
                    Ok(())
                }),
            )
            .map_err(|e| EngineError::Other(format!("CreateEnvironment: {e:?}")))?;
            rx.recv()
                .map_err(|_| EngineError::Other("Environment channel closed".into()))?
                .map_err(|e| EngineError::Other(format!("Environment: {e}")))?
        };

        // 2. CompositionController bound to the host HWND. The regular
        //    `CreateCoreWebView2Controller` returns an
        //    `ICoreWebView2Controller` that has no input-injection API;
        //    `CompositionController` is the variant Microsoft documents
        //    for hosted-rendering / off-screen scenarios where the
        //    embedder wants to drive `SendMouseInput`. Both interfaces
        //    are implemented by the same COM object — we keep both
        //    handles, casting at creation, so the rest of the file
        //    keeps its `controller.SetBounds` / `controller.CoreWebView2`
        //    pattern unchanged.
        let env3: ICoreWebView2Environment3 = environment
            .cast::<ICoreWebView2Environment3>()
            .map_err(|e| EngineError::Other(format!("cast Environment3: {e}")))?;
        //
        //    An isolated session's pages open in an InPrivate profile
        //    named after the session: pages of the session share it,
        //    nothing else does, and WebView2 deletes its data once the
        //    last page using it closes.
        let iso_options = match store.key() {
            None => None,
            Some(key) => Some(isolated_controller_options(&environment, key)?),
        };
        let comp_controller: ICoreWebView2CompositionController = {
            let (tx, rx) = mpsc::channel();
            let env = env3.clone();
            CreateCoreWebView2CompositionControllerCompletedHandler::wait_for_async_operation(
                Box::new(move |handler| unsafe {
                    match iso_options {
                        None => env.CreateCoreWebView2CompositionController(parent, &handler),
                        Some((env10, options)) => env10
                            .CreateCoreWebView2CompositionControllerWithOptions(
                                parent, &options, &handler,
                            ),
                    }
                    .map_err(webview2_com::Error::WindowsError)
                }),
                Box::new(move |error_code, controller| {
                    error_code?;
                    tx.send(controller.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                        .map_err(|_| windows::core::Error::from(E_POINTER))?;
                    Ok(())
                }),
            )
            .map_err(|e| EngineError::Other(format!("CreateCompositionController: {e:?}")))?;
            rx.recv()
                .map_err(|_| EngineError::Other("Controller channel closed".into()))?
                .map_err(|e| EngineError::Other(format!("Controller: {e}")))?
        };
        let origin = ViewOrigin {
            environment: environment.clone(),
            profile_key: store.key().map(str::to_string),
            proxied: matches!(store, DataStore::Proxied { .. }),
        };
        let page = host_view(&origin, parent, comp_controller, &self.popups)?;
        let web_view = page.web_view.clone();

        // 4. Navigate + wait for NavigationCompleted.
        let (tx, rx) = mpsc::channel();
        let handler = NavigationCompletedEventHandler::create(Box::new(move |_sender, _args| {
            let _ = tx.send(());
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe { web_view.add_NavigationCompleted(&handler, &raw mut token) }
            .map_err(|e| EngineError::Other(format!("add_NavigationCompleted: {e}")))?;
        let url_pwstr = pwstr_from_str(url);
        unsafe { web_view.Navigate(windows::core::PCWSTR(url_pwstr.0)) }
            .map_err(|e| EngineError::Other(format!("Navigate: {e}")))?;
        webview2_com::wait_with_pump(rx)
            .map_err(|e| EngineError::Other(format!("wait_with_pump: {e:?}")))?;
        unsafe { web_view.remove_NavigationCompleted(token) }
            .map_err(|e| EngineError::Other(format!("remove_NavigationCompleted: {e}")))?;

        let handle = self.alloc_handle();
        page.me.set(Some(handle));
        self.pages.insert(handle, page);
        Ok(handle)
    }

    fn navigate(&mut self, page: PageHandle, url: &str) -> EngineResult<()> {
        // Step 4 of `open`, against a WebView that already exists. The
        // document-start scripts survive the navigation, so this only
        // has to navigate and wait for the completion event.
        let web_view = self.page_mut(page)?.web_view.clone();
        let (tx, rx) = mpsc::channel();
        let handler = NavigationCompletedEventHandler::create(Box::new(move |_sender, _args| {
            let _ = tx.send(());
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe { web_view.add_NavigationCompleted(&handler, &raw mut token) }
            .map_err(|e| EngineError::Other(format!("add_NavigationCompleted: {e}")))?;
        let url_pwstr = pwstr_from_str(url);
        let navigated = unsafe { web_view.Navigate(windows::core::PCWSTR(url_pwstr.0)) }
            .map_err(|e| EngineError::Other(format!("Navigate: {e}")));
        let waited = navigated.and_then(|()| {
            webview2_com::wait_with_pump(rx)
                .map_err(|e| EngineError::Other(format!("wait_with_pump: {e:?}")))
        });
        // Unsubscribe whether or not the navigation worked, so a
        // failed goto does not leave a handler on the page firing into
        // a dropped channel for every later navigation.
        unsafe { web_view.remove_NavigationCompleted(token) }
            .map_err(|e| EngineError::Other(format!("remove_NavigationCompleted: {e}")))?;
        waited
    }

    fn add_init_script(&mut self, page: PageHandle, source: &str) -> EngineResult<()> {
        let web_view = self.page_mut(page)?.web_view.clone();
        add_init_script(&web_view, source)?;
        Ok(())
    }

    fn enable_webauthn(&mut self, page: PageHandle) -> EngineResult<()> {
        // Document-start script for later loads, as on macOS, then the
        // document already loaded.
        let web_view = self.page_mut(page)?.web_view.clone();
        add_init_script(&web_view, crate::backend::common::WEBAUTHN_JS)?;
        execute_script(&web_view, crate::backend::common::WEBAUTHN_JS)?;
        Ok(())
    }

    fn close(&mut self, page: PageHandle) -> EngineResult<()> {
        if let Some(p) = self.pages.remove(&page) {
            // Closing the controller releases the WebView2 process
            // entry. Errors here are best-effort.
            let _ = unsafe { p.controller.Close() };
        }
        Ok(())
    }

    fn snapshot(&mut self, page: PageHandle) -> EngineResult<Tree> {
        let p = self.page_mut(page)?;
        let json = execute_script(&p.web_view, SNAPSHOT_JS)?;
        crate::backend::common::parse_snapshot(&json).map_err(EngineError::Other)
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
        // A click on a ref is a native one (isTrusted), as on macOS.
        if let (ActTarget::Ref(r), Action::Click) = (&target, &action) {
            let at = crate::backend::common::ref_click_point(
                |js, _budget| execute_script(&web_view, js),
                *r,
            )?;
            if let Some((x, y)) = at {
                return self.cursor_op(page, CursorOp::ClickAt { x, y }, mode);
            }
        }
        crate::backend::common::run_act(
            move |js, _budget| execute_script(&web_view, js),
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
            |js, _budget| execute_script(&web_view, js),
            &cond,
            budget,
            || {
                // No labeled-break loop here — the wait body polls
                // every 150ms via `Duration::from_millis(150)` slice
                // in `run_wait`. Between polls we need to pump the
                // Win32 message loop so WebView2 callbacks (script
                // completions, etc.) make progress. A short
                // PeekMessage / DispatchMessage loop is enough.
                use windows::Win32::UI::WindowsAndMessaging::{
                    DispatchMessageW, PeekMessageW, MSG, PM_REMOVE,
                };
                let mut msg = MSG::default();
                unsafe {
                    while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        DispatchMessageW(&raw const msg);
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            },
        )
    }

    fn capture(&mut self, page: PageHandle, _scope: CaptureScope) -> EngineResult<PathBuf> {
        let captures_dir = self.captures_dir.clone().unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&captures_dir);
        let path = captures_dir.join(format!("capture-{}.png", page.0));

        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        let stream = unsafe {
            windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal(
                windows::Win32::Foundation::HGLOBAL(std::ptr::null_mut()),
                true,
            )
        }
        .map_err(|e| EngineError::Other(format!("CreateStreamOnHGlobal: {e}")))?;

        let (tx, rx) = mpsc::channel();
        let stream_for_handler = stream.clone();
        CapturePreviewCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                web_view
                    .CapturePreview(
                        COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
                        &stream_for_handler,
                        &handler,
                    )
                    .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |error_code| {
                error_code?;
                let _ = tx.send(());
                Ok(())
            }),
        )
        .map_err(|e| EngineError::Other(format!("CapturePreview: {e:?}")))?;
        rx.recv()
            .map_err(|_| EngineError::Other("CapturePreview channel closed".into()))?;

        // Read the IStream into bytes and write to disk.
        unsafe {
            stream
                .Seek(0, windows::Win32::System::Com::STREAM_SEEK_SET, None)
                .map_err(|e| EngineError::Other(format!("Stream Seek: {e}")))?;
        }
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let mut read = 0u32;
            let res = unsafe {
                stream.Read(
                    chunk.as_mut_ptr().cast(),
                    u32::try_from(chunk.len()).unwrap_or(u32::MAX),
                    Some(&raw mut read),
                )
            };
            res.ok()
                .map_err(|e| EngineError::Other(format!("Stream Read: {e}")))?;
            if read == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..read as usize]);
        }
        std::fs::write(&path, &buf)
            .map_err(|e| EngineError::Other(format!("write capture: {e}")))?;
        Ok(path)
    }

    fn layout(&mut self, page: PageHandle, refs: &[Ref]) -> EngineResult<Vec<LayoutBox>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        crate::backend::common::run_layout(move |js, _budget| execute_script(&web_view, js), refs)
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
            |js, _budget| execute_script(&web_view, js),
            || {
                // Pump the Win32 message loop between polls so the
                // in-page fetch's script completions make progress —
                // same reasoning as `wait`.
                use windows::Win32::UI::WindowsAndMessaging::{
                    DispatchMessageW, PeekMessageW, MSG, PM_REMOVE,
                };
                let mut msg = MSG::default();
                unsafe {
                    while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        DispatchMessageW(&raw const msg);
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
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
            move |js, _budget| execute_script(&web_view, js),
            &native,
        )
    }

    fn set_viewport(&mut self, page: PageHandle, viewport: Viewport) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        unsafe {
            p.controller.SetBounds(RECT {
                left: 0,
                top: 0,
                right: i32::try_from(viewport.width).unwrap_or(1280),
                bottom: i32::try_from(viewport.height).unwrap_or(800),
            })
        }
        .map_err(|e| EngineError::Other(format!("SetBounds: {e}")))?;
        Ok(())
    }

    fn crash(&mut self, page: PageHandle) -> EngineResult<()> {
        let web_view = self.page_mut(page)?.web_view.clone();
        // Not `call_cdp`: the renderer dies before it answers, so waiting
        // for the reply would pump messages forever. ProcessFailed
        // reports the crash.
        let done = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_, _| Ok(())));
        let m = pwstr_from_str("Page.crash");
        let p = pwstr_from_str("{}");
        unsafe {
            web_view.CallDevToolsProtocolMethod(
                windows::core::PCWSTR(m.0),
                windows::core::PCWSTR(p.0),
                &done,
            )
        }
        .map_err(|e| EngineError::Other(format!("Page.crash: {e}")))?;
        Ok(())
    }

    fn drop_data(
        &mut self,
        page: PageHandle,
        x: f64,
        y: f64,
        data: &crate::engine::DropData,
    ) -> EngineResult<bool> {
        let (web_view, comp) = {
            let p = self.page_mut(page)?;
            (p.web_view.clone(), p.comp_controller.clone())
        };
        let shim = crate::backend::drop_shim::install_js(data);
        if let Some(js) = &shim {
            execute_script(&web_view, js)?;
        }
        crate::backend::wv2_drop::drop_data(&comp, point_at(vs_humanize::Point { x, y }), data)?;
        // The shim's capture listener takes the pending data on the
        // trusted drop: consumed means the page got `drop`.
        let mut accepted = shim.is_none();
        for _ in 0..10 {
            if accepted {
                break;
            }
            accepted = execute_script(&web_view, crate::backend::drop_shim::CONSUMED_JS)
                .is_ok_and(|r| r.contains("true"));
            std::thread::sleep(Duration::from_millis(50));
        }
        self.follow_new_window(page, false)?;
        Ok(accepted)
    }

    fn set_zoom(&mut self, page: PageHandle, zoom: f64) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        unsafe { p.controller.SetZoomFactor(zoom) }
            .map_err(|e| EngineError::Other(format!("SetZoomFactor: {e}")))
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
        // Cookies: host-side via ICoreWebView2CookieManager so
        // HttpOnly entries are captured. localStorage/sessionStorage:
        // JS shim.
        let cookies = cookies::get_all_cookies(&web_view)?;
        let storage = crate::backend::common::run_save_storage_only(
            move |js, _budget| execute_script(&web_view, js),
            || {
                // Same pump as `run_wait`: WebView2's callbacks only
                // run when this thread dispatches its messages.
                use windows::Win32::UI::WindowsAndMessaging::{
                    DispatchMessageW, PeekMessageW, MSG, PM_REMOVE,
                };
                let mut msg = MSG::default();
                unsafe {
                    while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        DispatchMessageW(&raw const msg);
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
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
            move |js, _budget| execute_script(&web_view, js),
            &parsed.local_storage,
            &parsed.session_storage,
            &parsed.indexed_db,
            || {
                // Same pump as `run_wait`: WebView2's callbacks only
                // run when this thread dispatches its messages.
                use windows::Win32::UI::WindowsAndMessaging::{
                    DispatchMessageW, PeekMessageW, MSG, PM_REMOVE,
                };
                let mut msg = MSG::default();
                unsafe {
                    while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        DispatchMessageW(&raw const msg);
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
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
        crate::backend::common::run_eval(move |js, _budget| execute_script(&web_view, js), expr)
    }

    fn eval_js_in_frame(
        &mut self,
        page: PageHandle,
        frame: &str,
        expr: &str,
    ) -> EngineResult<crate::inspector::EvalResult> {
        let frames = self.page_mut(page)?.frames.clone();
        let target =
            crate::backend::webview2_extras::find_frame(&frames, frame).ok_or_else(|| {
                EngineError::NotFound {
                    kind: "frame",
                    id: frame.to_string(),
                }
            })?;
        crate::backend::common::run_eval(
            move |js, _budget| crate::backend::webview2_extras::execute_in_frame(&target, js),
            expr,
        )
    }

    fn set_popups(&mut self, page: PageHandle, on: bool) -> EngineResult<()> {
        self.page_mut(page)?.popups_on.set(on);
        Ok(())
    }

    fn popup_feed(&mut self) -> Option<std::sync::Arc<crate::engine::PopupFeed>> {
        Some(self.popups.borrow().feed.clone())
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
        crate::backend::common::run_storage(move |js, _budget| execute_script(&web_view, js), scope)
    }

    fn scripts(&mut self, page: PageHandle) -> EngineResult<Vec<crate::inspector::ScriptEntry>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        crate::backend::common::run_scripts(move |js, _budget| execute_script(&web_view, js))
    }

    fn script_source(
        &mut self,
        page: PageHandle,
        seq: u64,
    ) -> EngineResult<Option<crate::inspector::ScriptSource>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        crate::backend::common::run_script_source(
            move |js, _budget| execute_script(&web_view, js),
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
            move |js, _budget| execute_script(&web_view, js),
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
        crate::backend::common::run_performance(move |js, _budget| execute_script(&web_view, js))
    }

    fn main_response(
        &mut self,
        page: PageHandle,
    ) -> EngineResult<Option<crate::inspector::MainResponse>> {
        Ok(self.page_mut(page)?.responses.borrow().main_response())
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
        self.forget_new_window(page)?;
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        let (base_delay, span) = match mode {
            InputMode::Robotic => (0_u64, 0_u64),
            InputMode::Careful => (120, 0),
            InputMode::Human => (45, 18),
        };
        let mut jitter = text
            .chars()
            .map(|c| c as u64)
            .fold(0x9e37_79b9_u64, |a, c| a.wrapping_mul(31).wrapping_add(c))
            | 1;

        for ch in text.chars() {
            let spelling = cdp_key(ch);
            let mut down = serde_json::json!({
                "type": "keyDown",
                "key": spelling.key,
                "text": spelling.text,
                "unmodifiedText": spelling.text,
                "windowsVirtualKeyCode": spelling.virtual_key,
            });
            let mut up = serde_json::json!({
                "type": "keyUp",
                "key": spelling.key,
                "windowsVirtualKeyCode": spelling.virtual_key,
            });
            if let Some(code) = spelling.code {
                down["code"] = serde_json::Value::from(code);
                up["code"] = serde_json::Value::from(code);
            }
            call_cdp(&web_view, "Input.dispatchKeyEvent", &down.to_string())?;
            let hold = if base_delay == 0 { 1 } else { base_delay / 3 };
            std::thread::sleep(Duration::from_millis(hold.max(1)));
            call_cdp(&web_view, "Input.dispatchKeyEvent", &up.to_string())?;
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
                std::thread::sleep(Duration::from_millis(wait.max(1)));
            }
        }
        self.follow_new_window(page, false)
    }

    fn cursor_op(&mut self, page: PageHandle, op: CursorOp, mode: InputMode) -> EngineResult<()> {
        self.forget_new_window(page)?;
        let p = self.page_mut(page)?;
        let comp = p.comp_controller.clone();
        let humanize_mode = match mode {
            InputMode::Human => vs_humanize::InputMode::Human,
            InputMode::Careful => vs_humanize::InputMode::Careful,
            InputMode::Robotic => vs_humanize::InputMode::Robotic,
        };
        let start = p.last_mouse.get();
        let seed = wv2_humanize_seed(op);
        let landed = match op {
            CursorOp::MoveTo { x, y } | CursorOp::HoverAt { x, y } => wv2_move_along_path(
                &comp,
                start,
                vs_humanize::Point { x, y },
                humanize_mode,
                seed,
            )?,
            CursorOp::ClickAt { x, y } => {
                let landed = wv2_move_along_path(
                    &comp,
                    start,
                    vs_humanize::Point { x, y },
                    humanize_mode,
                    seed,
                )?;
                wv2_press_release(&comp, landed)?;
                landed
            }
            CursorOp::Drag { x1, y1, x2, y2 } => {
                let start_pt = vs_humanize::Point { x: x1, y: y1 };
                let target = vs_humanize::Point { x: x2, y: y2 };
                let pre = wv2_move_along_path(&comp, start, start_pt, humanize_mode, seed)?;
                wv2_send_mouse(&comp, COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN, pre)?;
                std::thread::sleep(Duration::from_millis(15));
                let landed = wv2_move_along_path(&comp, pre, target, humanize_mode, seed)?;
                wv2_send_mouse(&comp, COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP, landed)?;
                // After the OS-level drag, fire the HTML5 DragEvent
                // chain in JS so react-dnd / React-Flow HTML5 targets
                // observe the drop.
                let html5_js = crate::backend::common::build_html5_drag_js(x1, y1, x2, y2);
                let web_view = p.web_view.clone();
                let _ = execute_script(&web_view, &html5_js);
                landed
            }
            CursorOp::Click { x, y, opts } => {
                let landed = wv2_move_along_path(
                    &comp,
                    start,
                    vs_humanize::Point { x, y },
                    humanize_mode,
                    seed,
                )?;
                let web_view = p.web_view.clone();
                cdp_click(&web_view, landed, opts)?;
                landed
            }
            CursorOp::Wheel { x, y, dx, dy } => {
                let landed = wv2_move_along_path(
                    &comp,
                    start,
                    vs_humanize::Point { x, y },
                    humanize_mode,
                    seed,
                )?;
                wv2_wheel(&comp, landed, dx, dy)?;
                landed
            }
        };
        p.last_mouse.set(landed);
        self.follow_new_window(
            page,
            matches!(op, CursorOp::ClickAt { .. } | CursorOp::Click { .. }),
        )
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
            name: "webview2",
            version: "Windows WebView2 (webview2-com 0.39)",
        }
    }
}
