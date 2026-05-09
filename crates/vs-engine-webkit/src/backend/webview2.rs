//! Windows backend — Microsoft WebView2 (Edge-based) via
//! `webview2-com` + `windows-rs`.
//!
//! # Threading model
//!
//! WebView2 requires a Win32 message pump on the thread that owns
//! each `ICoreWebView2Controller`. Methods on [`Webview2Backend`] must
//! be called from that thread; the type is `!Send`. Production wiring
//! (in `vs-cli::serve` for `cfg(target_os = "windows")`) creates a
//! hidden parent HWND on the OS main thread, runs `GetMessageW` /
//! `DispatchMessageW` there, and dispatches engine calls onto it via
//! [`crate::runtime::MainThreadDispatcher`] — same shape as the
//! macOS NSRunLoop / Linux GLib MainContext paths.
//!
//! # Status
//!
//! Full-surface implementation, written from the WebView2 docs +
//! `webview2-com` sample. **Not verified on macOS** — this file is
//! `cfg(target_os = "windows")` and only compiles when targeting
//! Windows with the WebView2 SDK + Runtime installed. The CI matrix
//! at `.github/workflows/m6.yml` runs the full M6 suite on
//! `windows-latest`; the maintainer manually verifies the run before
//! flipping the Windows column to `yes` in `REALITY_CHECK.md`.
//!
//! Capability flags follow the same per-page-bool + aggregate pattern
//! Mac (`webkit/mod.rs`) and Linux (`wpe.rs`) use: `inspector_*`
//! goes `true` only when the install path actually succeeded.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use vs_protocol::{Ref, Tree};

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2, ICoreWebView2Controller, ICoreWebView2Environment,
    COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
};
use webview2_com::{
    pwstr_from_str, take_pwstr, AddScriptToExecuteOnDocumentCreatedCompletedHandler,
    CapturePreviewCompletedHandler, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, ExecuteScriptCompletedHandler,
    NavigationCompletedEventHandler, WebMessageReceivedEventHandler,
};
use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::{E_POINTER, HWND, RECT};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, RegisterClassW, HWND_MESSAGE, WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPED,
};

use crate::backend::inspector_bridge::{
    self, InspectorSlots, NetworkIngestSlot, CONSOLE_HANDLER, NETWORK_HANDLER,
};
use crate::engine::{
    ActTarget, Action, AuthBlob, CaptureScope, Engine, EngineCapabilities, EngineError,
    EngineResult, LayoutBox, PageHandle, Viewport, WaitCondition,
};
// =============================================================================
// JS payload shared with Mac / Linux backends.
// =============================================================================

use super::common::SNAPSHOT_DOM_WALKER_JS as SNAPSHOT_JS;

// =============================================================================
// Per-page state
// =============================================================================

#[allow(dead_code)]
struct W2Page {
    /// The COM controller drives the WebView2 in a hosted window.
    /// Held strong so the COM object isn't released while the page is
    /// open.
    controller: ICoreWebView2Controller,
    web_view: ICoreWebView2,
    parent_hwnd: HWND,
    inspector: InspectorSlots,
    /// True iff the inspector install path (user script + script
    /// message subscription) registered successfully for this page.
    inspector_installed: bool,
}

// =============================================================================
// Backend
// =============================================================================

/// WebView2 backend. Construct after `CoInitializeEx` on the message
/// thread; subsequent calls assume that same thread.
pub struct Webview2Backend {
    pages: HashMap<PageHandle, W2Page>,
    next_handle: u64,
    captures_dir: Option<PathBuf>,
    /// Hidden parent HWND used as the root of every WebView2
    /// controller's host window. Lazily initialized on the first
    /// `open` call.
    parent_hwnd: Option<HWND>,
}

impl Default for Webview2Backend {
    fn default() -> Self {
        Self::new()
    }
}

/// `WNDCLASSW::lpfnWndProc` defaults to `None` — a NULL function
/// pointer in the registered class. WebView2's controller posts
/// messages to our hidden host HWND during `CreateCoreWebView2Controller`,
/// at which point Windows dispatches by calling that NULL pointer
/// (CPU jumps to address 0, daemon dies with STATUS_ACCESS_VIOLATION).
/// This stateless shim forwards every message to `DefWindowProcW`,
/// the standard "do the default thing" handler.
unsafe extern "system" fn wnd_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::DefWindowProcW;
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

impl Webview2Backend {
    /// Build a backend. Caller must already have initialized COM
    /// (`CoInitializeEx` STA) on this thread before constructing.
    /// Production wiring lives in `vs-cli::serve` on Windows.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pages: HashMap::new(),
            next_handle: 1,
            captures_dir: None,
            parent_hwnd: None,
        }
    }

    /// Pin the directory where `capture` writes PNGs.
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

    fn page_mut(&mut self, h: PageHandle) -> EngineResult<&mut W2Page> {
        self.pages.get_mut(&h).ok_or(EngineError::NotFound {
            kind: "page",
            id: h.0.to_string(),
        })
    }

    /// Lazily create the message-only parent HWND that hosts every
    /// WebView2 controller. Idempotent.
    fn parent_hwnd(&mut self) -> EngineResult<HWND> {
        if let Some(h) = self.parent_hwnd {
            return Ok(h);
        }
        // SAFETY: CoInitializeEx is required by the caller per the
        // module docs; we ignore RPC_E_CHANGED_MODE (already
        // initialized as STA on this thread).
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        // `wnd_proc` (module-level) is what makes `lpfnWndProc`
        // non-NULL — see its docstring for why a NULL there crashes
        // the daemon with STATUS_ACCESS_VIOLATION.
        let class_name: HSTRING = HSTRING::from("vs-webview2-host");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        // SAFETY: registering an idempotent class — duplicate
        // registrations under the same name are harmless on Windows.
        unsafe {
            let _atom = RegisterClassW(&raw const class);
        }
        // SAFETY: creating a message-only window (HWND_MESSAGE
        // parent) — never visible, never gets focus, used only as a
        // host for the WebView2 controller.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                windows::core::PCWSTR(class_name.as_ptr()),
                windows::core::PCWSTR::null(),
                WS_OVERLAPPED,
                0,
                0,
                1280,
                800,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .map_err(|e| EngineError::Other(format!("CreateWindowExW: {e}")))?;
        self.parent_hwnd = Some(hwnd);
        Ok(hwnd)
    }
}

// =============================================================================
// COM helpers — wait_for_async_operation wrappers.
// =============================================================================

/// Synchronously call `webview.ExecuteScript(js)` and return the
/// JSON-encoded result. Pumps the Win32 message loop while waiting.
fn execute_script(web_view: &ICoreWebView2, js: &str) -> EngineResult<String> {
    let (tx, rx) = mpsc::channel();
    let js_owned = js.to_string();
    let web_view_owned: ICoreWebView2 = web_view.clone();
    ExecuteScriptCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            let pwstr = pwstr_from_str(&js_owned);
            unsafe { web_view_owned.ExecuteScript(windows::core::PCWSTR(pwstr.0), &handler) }
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |error_code, result_json| {
            error_code?;
            let _ = tx.send(result_json);
            Ok(())
        }),
    )
    .map_err(|e| EngineError::Other(format!("ExecuteScript: {e:?}")))?;
    let json = rx
        .recv()
        .map_err(|_| EngineError::Other("ExecuteScript: channel closed".into()))?;
    Ok(json)
}

/// Add `js` to the document-start initializer for `web_view`.
/// Returns the script id, which the caller stores in case we want to
/// remove it later.
fn add_init_script(web_view: &ICoreWebView2, js: &str) -> EngineResult<String> {
    let (tx, rx) = mpsc::channel();
    let js_owned = js.to_string();
    let web_view_owned: ICoreWebView2 = web_view.clone();
    AddScriptToExecuteOnDocumentCreatedCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            let pwstr = pwstr_from_str(&js_owned);
            unsafe {
                web_view_owned
                    .AddScriptToExecuteOnDocumentCreated(windows::core::PCWSTR(pwstr.0), &handler)
            }
            .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |error_code, script_id| {
            error_code?;
            let _ = tx.send(script_id);
            Ok(())
        }),
    )
    .map_err(|e| EngineError::Other(format!("AddScriptToExecuteOnDocumentCreated: {e:?}")))?;
    rx.recv()
        .map_err(|_| EngineError::Other("AddScript: channel closed".into()))
}

/// Run the inspector install path: subscribe `WebMessageReceived`
/// (one handler that switches on a `kind` field), then add the
/// shared inspector_capture.js as a document-start script. Returns
/// `false` if the env var `VS_DISABLE_INSPECTOR=1` is set, mirroring
/// Mac/Linux for the capability-gate test.
fn install_inspector(web_view: &ICoreWebView2, slots: &InspectorSlots) -> bool {
    if std::env::var_os("VS_DISABLE_INSPECTOR").is_some() {
        return false;
    }
    let cs_console = slots.console.clone();
    let cs_network = slots.network.clone();
    let cs_details = slots.details.clone();
    let cs_pending = slots.pending.clone();

    // The shared bridge JS (in inspector_capture.js) calls
    // `window.webkit.messageHandlers.<name>.postMessage(json)` on
    // Mac/Linux. WebView2 only exposes `window.chrome.webview
    // .postMessage(...)` — one global channel — so we install a
    // shim that maps the `webkit.messageHandlers` API onto
    // chrome.webview by tagging the JSON with `__channel`.
    let shim = r"
        window.webkit = window.webkit || {};
        window.webkit.messageHandlers = window.webkit.messageHandlers || {};
        function __vsMakeHandler(name) {
            return {
                postMessage: function(msg) {
                    var s = (typeof msg === 'string') ? msg : JSON.stringify(msg);
                    var wrapped = JSON.stringify({ __channel: name, body: s });
                    window.chrome.webview.postMessage(wrapped);
                },
            };
        }
        window.webkit.messageHandlers.vsConsole = __vsMakeHandler('vsConsole');
        window.webkit.messageHandlers.vsNetwork = __vsMakeHandler('vsNetwork');
    ";
    if add_init_script(web_view, shim).is_err() {
        return false;
    }
    if add_init_script(web_view, inspector_bridge::SCRIPT).is_err() {
        return false;
    }

    let handler = WebMessageReceivedEventHandler::create(Box::new(move |_sender, args_opt| {
        let Some(args) = args_opt else {
            return Ok(());
        };
        let mut raw = PWSTR(std::ptr::null_mut());
        if unsafe { args.WebMessageAsJson(&raw mut raw) }.is_err() {
            return Ok(());
        }
        let outer = take_pwstr(raw);
        let v: serde_json::Value = match serde_json::from_str(&outer) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        // The shim wraps every host message as
        // `{__channel: name, body: <json-string>}`. Unwrap.
        let channel = v.get("__channel").and_then(|x| x.as_str()).unwrap_or("");
        let body = v.get("body").and_then(|x| x.as_str()).unwrap_or("");
        match channel {
            CONSOLE_HANDLER => {
                let mut buf = cs_console.borrow_mut();
                inspector_bridge::ingest_console(&mut buf, body);
            }
            NETWORK_HANDLER => {
                let mut entries = cs_network.borrow_mut();
                let mut details = cs_details.borrow_mut();
                let mut pending = cs_pending.borrow_mut();
                inspector_bridge::ingest_network(
                    NetworkIngestSlot {
                        entries: &mut entries,
                        details: &mut details,
                        pending: &mut pending,
                    },
                    body,
                );
            }
            _ => {}
        }
        Ok(())
    }));
    let mut token: i64 = 0;
    if unsafe { web_view.add_WebMessageReceived(&handler, &raw mut token) }.is_err() {
        return false;
    }
    true
}

// =============================================================================
// Engine impl
// =============================================================================

impl Engine for Webview2Backend {
    fn open(&mut self, url: &str) -> EngineResult<PageHandle> {
        let parent = self.parent_hwnd()?;

        // 1. Environment.
        let environment: ICoreWebView2Environment = {
            let (tx, rx) = mpsc::channel();
            CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
                Box::new(|handler| unsafe {
                    webview2_com::Microsoft::Web::WebView2::Win32::CreateCoreWebView2Environment(
                        &handler,
                    )
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

        // 2. Controller bound to the host HWND.
        let controller: ICoreWebView2Controller = {
            let (tx, rx) = mpsc::channel();
            let env = environment.clone();
            CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
                Box::new(move |handler| unsafe {
                    env.CreateCoreWebView2Controller(parent, &handler)
                        .map_err(webview2_com::Error::WindowsError)
                }),
                Box::new(move |error_code, controller| {
                    error_code?;
                    tx.send(controller.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                        .map_err(|_| windows::core::Error::from(E_POINTER))?;
                    Ok(())
                }),
            )
            .map_err(|e| EngineError::Other(format!("CreateController: {e:?}")))?;
            rx.recv()
                .map_err(|_| EngineError::Other("Controller channel closed".into()))?
                .map_err(|e| EngineError::Other(format!("Controller: {e}")))?
        };

        unsafe {
            controller.SetBounds(RECT {
                left: 0,
                top: 0,
                right: 1280,
                bottom: 800,
            })
        }
        .map_err(|e| EngineError::Other(format!("SetBounds: {e}")))?;

        let web_view: ICoreWebView2 = unsafe { controller.CoreWebView2() }
            .map_err(|e| EngineError::Other(format!("CoreWebView2: {e}")))?;

        // Pin the User-Agent to a current Safari string so anti-bot
        // fingerprinters don't flag the WebView2 default. Settings2
        // is the interface that exposes UserAgent — the base
        // ICoreWebView2Settings doesn't have it.
        unsafe {
            use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings2;
            use windows::core::Interface;
            let settings = web_view
                .Settings()
                .map_err(|e| EngineError::Other(format!("Settings: {e}")))?;
            if let Ok(s2) = settings.cast::<ICoreWebView2Settings2>() {
                let ua: Vec<u16> = crate::engine::DEFAULT_USER_AGENT
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let _ = s2.SetUserAgent(windows::core::PCWSTR(ua.as_ptr()));
            }
        }

        // 3. Install inspector bridge BEFORE Navigate so the
        //    document-start hook fires on the loaded page.
        let inspector = InspectorSlots::new(crate::inspector::DEFAULT_BUFFER_CAPACITY);
        let inspector_installed = install_inspector(&web_view, &inspector);

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
        self.pages.insert(
            handle,
            W2Page {
                controller,
                web_view,
                parent_hwnd: parent,
                inspector,
                inspector_installed,
            },
        );
        Ok(handle)
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
        super::common::parse_snapshot(&json).map_err(EngineError::Other)
    }

    fn act(&mut self, page: PageHandle, target: ActTarget, action: Action) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_act(
            move |js, _budget| execute_script(&web_view, js),
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
        super::common::run_layout(move |js, _budget| execute_script(&web_view, js), refs)
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

    fn save_auth(&mut self, page: PageHandle) -> EngineResult<AuthBlob> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_save_auth(move |js, _budget| execute_script(&web_view, js))
    }

    fn load_auth(&mut self, page: PageHandle, blob: &AuthBlob) -> EngineResult<()> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_load_auth(move |js, _budget| execute_script(&web_view, js), blob)
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
        super::common::run_eval(move |js, _budget| execute_script(&web_view, js), expr)
    }

    fn storage(
        &mut self,
        page: PageHandle,
        scope: crate::inspector::StorageScope,
    ) -> EngineResult<Vec<crate::inspector::StorageEntry>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_storage(move |js, _budget| execute_script(&web_view, js), scope)
    }

    fn scripts(&mut self, page: PageHandle) -> EngineResult<Vec<crate::inspector::ScriptEntry>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_scripts(move |js, _budget| execute_script(&web_view, js))
    }

    fn script_source(
        &mut self,
        page: PageHandle,
        seq: u64,
    ) -> EngineResult<Option<crate::inspector::ScriptSource>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_script_source(move |js, _budget| execute_script(&web_view, js), seq)
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
        super::common::run_performance(move |js, _budget| execute_script(&web_view, js))
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
            name: "webview2",
            version: "Windows WebView2 (webview2-com 0.39)",
        }
    }
}
