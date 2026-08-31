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
    ICoreWebView2, ICoreWebView2CompositionController, ICoreWebView2Controller,
    ICoreWebView2Environment, ICoreWebView2Environment3,
    COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, COREWEBVIEW2_MOUSE_EVENT_KIND,
    COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN, COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP,
    COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE, COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_NONE,
};
use webview2_com::{
    pwstr_from_str, take_pwstr, AddScriptToExecuteOnDocumentCreatedCompletedHandler,
    CapturePreviewCompletedHandler, CreateCoreWebView2CompositionControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, ExecuteScriptCompletedHandler,
    NavigationCompletedEventHandler, WebMessageReceivedEventHandler,
};
use windows::core::{Interface, HSTRING, PWSTR};
use windows::Win32::Foundation::{E_POINTER, HWND, POINT, RECT};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, RegisterClassW, HWND_MESSAGE, WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPED,
};

use crate::backend::inspector_bridge::{
    self, InspectorSlots, NetworkIngestSlot, CONSOLE_HANDLER, NETWORK_HANDLER,
};
use crate::engine::{
    ActTarget, Action, AuthBlob, CaptureScope, CursorOp, Download, DownloadEntry, DownloadSource,
    Engine, EngineCapabilities, EngineError, EngineResult, InputMode, LayoutBox, PageHandle,
    Viewport, WaitCondition,
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
    /// CompositionController is the v0.1.11 input-injection path:
    /// `SendMouseInput` only exists on this variant (the regular
    /// `ICoreWebView2Controller` has no input API). Held strong so
    /// the COM object isn't released while the page is open.
    comp_controller: ICoreWebView2CompositionController,
    /// `comp_controller` cast to the regular controller interface for
    /// `SetBounds`, `Close`, and `CoreWebView2` access. WebView2
    /// guarantees both interfaces are implemented by the same object.
    controller: ICoreWebView2Controller,
    web_view: ICoreWebView2,
    parent_hwnd: HWND,
    /// DirectComposition device — owns the visual tree the WebView2
    /// renders into. `Commit` was called once at setup; we hold the
    /// device so the visual tree stays alive for the page's lifetime.
    _dcomp_device: IDCompositionDevice,
    /// Composition target bound to `parent_hwnd`.
    _dcomp_target: IDCompositionTarget,
    /// Root visual passed to `comp_controller.SetRootVisualTarget`.
    _dcomp_visual: IDCompositionVisual,
    inspector: InspectorSlots,
    /// True iff the inspector install path (user script + script
    /// message subscription) registered successfully for this page.
    inspector_installed: bool,
    cookie_baseline: std::cell::RefCell<Option<Vec<super::auth::CookieData>>>,
    cookie_next_seq: std::cell::RefCell<u64>,
    /// Last known cursor position in WebView-local CSS px. Updated
    /// after every `cursor_op` so the next humanized lead-in starts
    /// where the previous one ended. Defaults to (0, 0) on `open`.
    last_mouse: std::cell::Cell<vs_humanize::Point>,
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

    /// Create a fresh message-only HWND to host this page's WebView2
    /// composition target. DirectComposition rejects a second
    /// `CreateTargetForHwnd` on a window that already has a target
    /// (`DCOMPOSITION_ERROR_WINDOW_ALREADY_COMPOSED`), so every page
    /// gets its own HWND under the same shared class.
    fn create_host_hwnd(&mut self) -> EngineResult<HWND> {
        // SAFETY: CoInitializeEx is required by the caller per the
        // module docs; we ignore RPC_E_CHANGED_MODE (already
        // initialized as STA on this thread).
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        // `wnd_proc` (module-level) is what makes `lpfnWndProc`
        // non-NULL — see its docstring for why a NULL there crashes
        // the daemon with STATUS_ACCESS_VIOLATION. Class registration
        // is idempotent on Windows; duplicate `RegisterClassW` calls
        // under the same name are harmless.
        let class_name: HSTRING = HSTRING::from("vs-webview2-host");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        unsafe {
            let _atom = RegisterClassW(&raw const class);
        }
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
        // Remember the most recent HWND only so `Default::default()`
        // is satisfied; per-page HWNDs live on `W2Page::parent_hwnd`.
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
                    // Pass an object, not a string. postMessage of a
                    // string makes WebMessageAsJson on the host side
                    // return a JSON-encoded string literal that our
                    // host parser cannot field-access. The object
                    // form makes WebMessageAsJson return the object,
                    // and our handler reads __channel + body off it.
                    window.chrome.webview.postMessage({ __channel: name, body: s });
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
    // The COM bring-up (environment → composition controller →
    // DirectComposition device/target/visual → settings → navigate)
    // is one linear sequence; splitting it would just scatter the
    // handle plumbing.
    #[allow(clippy::too_many_lines)]
    fn open(&mut self, url: &str) -> EngineResult<PageHandle> {
        let parent = self.create_host_hwnd()?;

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
        let comp_controller: ICoreWebView2CompositionController = {
            let (tx, rx) = mpsc::channel();
            let env = env3.clone();
            CreateCoreWebView2CompositionControllerCompletedHandler::wait_for_async_operation(
                Box::new(move |handler| unsafe {
                    env.CreateCoreWebView2CompositionController(parent, &handler)
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
        let controller: ICoreWebView2Controller = comp_controller
            .cast::<ICoreWebView2Controller>()
            .map_err(|e| EngineError::Other(format!("cast Controller: {e}")))?;

        // 2b. DirectComposition wiring. CompositionController needs a
        //     root visual to render into — we create one bound to the
        //     hidden parent HWND, hand it to the controller, and commit
        //     once. The device + target + visual are stored on the page
        //     so their refcounts outlive the open() call.
        let dcomp_device: IDCompositionDevice = unsafe {
            DCompositionCreateDevice2::<_, IDCompositionDevice>(None)
                .map_err(|e| EngineError::Other(format!("DCompositionCreateDevice2: {e}")))?
        };
        let dcomp_target: IDCompositionTarget = unsafe {
            dcomp_device
                .CreateTargetForHwnd(parent, true)
                .map_err(|e| EngineError::Other(format!("CreateTargetForHwnd: {e}")))?
        };
        let dcomp_visual: IDCompositionVisual = unsafe {
            dcomp_device
                .CreateVisual()
                .map_err(|e| EngineError::Other(format!("CreateVisual: {e}")))?
        };
        unsafe {
            dcomp_target
                .SetRoot(&dcomp_visual)
                .map_err(|e| EngineError::Other(format!("SetRoot: {e}")))?;
            comp_controller
                .SetRootVisualTarget(&dcomp_visual)
                .map_err(|e| EngineError::Other(format!("SetRootVisualTarget: {e}")))?;
            dcomp_device
                .Commit()
                .map_err(|e| EngineError::Other(format!("DComposition Commit: {e}")))?;
        }

        unsafe {
            controller.SetBounds(RECT {
                left: 0,
                top: 0,
                right: 1280,
                bottom: 800,
            })
        }
        .map_err(|e| EngineError::Other(format!("SetBounds: {e}")))?;

        // Mark the (offscreen, composition-hosted) controller visible.
        // `IsVisible` defaults to FALSE, which suspends rendering — a
        // single `CapturePreview` catches the initial post-load frame,
        // but after a `viewport`/`SetBounds` the WebView must re-render
        // at the new size, which an invisible controller never does, so
        // the next `CapturePreview` completion handler never fires and
        // the call hangs (seen as the sequential-capture wedge on
        // Windows). Visible + offscreen composition = continuous frames
        // CapturePreview can always read, with nothing shown on screen.
        unsafe { controller.SetIsVisible(true) }
            .map_err(|e| EngineError::Other(format!("SetIsVisible: {e}")))?;

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
        // Download capture, same shim as Mac / Linux.
        // `AddScriptToExecuteOnDocumentCreated` applies to every frame,
        // which is what the in-iframe viewer case needs.
        let _ = add_init_script(&web_view, super::common::DOWNLOAD_SHIM_JS);

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
                comp_controller,
                controller,
                web_view,
                parent_hwnd: parent,
                _dcomp_device: dcomp_device,
                _dcomp_target: dcomp_target,
                _dcomp_visual: dcomp_visual,
                inspector,
                inspector_installed,
                cookie_baseline: std::cell::RefCell::new(None),
                cookie_next_seq: std::cell::RefCell::new(0),
                last_mouse: std::cell::Cell::new(vs_humanize::Point { x: 0.0, y: 0.0 }),
            },
        );
        Ok(handle)
    }

    fn navigate(&mut self, _page: PageHandle, _url: &str) -> EngineResult<()> {
        Err(EngineError::NotImplemented {
            engine: "webview2",
            primitive: "navigate",
        })
    }

    fn enable_webauthn(&mut self, _page: PageHandle) -> EngineResult<()> {
        Err(EngineError::NotImplemented {
            engine: "webview2",
            primitive: "enable_webauthn",
        })
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

    fn act(
        &mut self,
        page: PageHandle,
        target: ActTarget,
        action: Action,
        _mode: InputMode,
    ) -> EngineResult<()> {
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

    fn download(
        &mut self,
        page: PageHandle,
        source: DownloadSource,
        budget: Duration,
    ) -> EngineResult<Download> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_download(
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
        )
    }

    fn download_list(&mut self, page: PageHandle) -> EngineResult<Vec<DownloadEntry>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_download_list(move |js, _budget| execute_script(&web_view, js))
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
        // Cookies: host-side via ICoreWebView2CookieManager so
        // HttpOnly entries are captured. localStorage/sessionStorage:
        // JS shim.
        let cookies = wv2_cookies::get_all_cookies(&web_view)?;
        let storage =
            super::common::run_save_storage_only(move |js, _budget| execute_script(&web_view, js))?;
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
        wv2_cookies::set_cookies(&web_view, &parsed.cookies)?;
        super::common::run_load_storage_only(
            move |js, _budget| execute_script(&web_view, js),
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
        super::common::run_eval(move |js, _budget| execute_script(&web_view, js), expr)
    }

    fn storage(
        &mut self,
        page: PageHandle,
        scope: crate::inspector::StorageScope,
    ) -> EngineResult<Vec<crate::inspector::StorageEntry>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        if matches!(scope, crate::inspector::StorageScope::Cookies) {
            let cookies = wv2_cookies::get_all_cookies(&web_view)?;
            return Ok(cookies
                .iter()
                .map(super::common::cookie_to_storage_entry)
                .collect());
        }
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

    fn cookie_events(
        &mut self,
        page: PageHandle,
    ) -> EngineResult<Vec<crate::inspector::CookieEvent>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        let current = wv2_cookies::get_all_cookies(&web_view)?;
        let previous = p.cookie_baseline.borrow().clone();
        let mut seq = p.cookie_next_seq.borrow_mut();
        let events = super::common::diff_cookies(previous.as_deref(), &current, &mut seq);
        *p.cookie_baseline.borrow_mut() = Some(current);
        Ok(events)
    }

    fn cursor_op(&mut self, page: PageHandle, op: CursorOp, mode: InputMode) -> EngineResult<()> {
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
                let html5_js = super::common::build_html5_drag_js(x1, y1, x2, y2);
                let web_view = p.web_view.clone();
                let _ = execute_script(&web_view, &html5_js);
                landed
            }
        };
        p.last_mouse.set(landed);
        Ok(())
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

// =============================================================================
// Host-side cookie save/load via ICoreWebView2CookieManager.
// =============================================================================
//
// Mirror of the macOS `webkit::cookie_store` and Linux `wpe_cookies`
// modules. `document.cookie` can't see or write `HttpOnly`; the
// `ICoreWebView2CookieManager` API can. The v0.1.2 fix routes auth
// save/load through this path on every backend.

// =============================================================================
// Cursor primitive helpers (SendMouseInput on the CompositionController)
// =============================================================================

fn wv2_humanize_seed(op: CursorOp) -> u64 {
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

#[allow(clippy::cast_possible_truncation)]
fn point_at(p: vs_humanize::Point) -> POINT {
    POINT {
        x: p.x.round() as i32,
        y: p.y.round() as i32,
    }
}

fn wv2_send_mouse(
    comp: &ICoreWebView2CompositionController,
    kind: COREWEBVIEW2_MOUSE_EVENT_KIND,
    point: vs_humanize::Point,
) -> EngineResult<()> {
    unsafe {
        comp.SendMouseInput(
            kind,
            COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_NONE,
            0,
            point_at(point),
        )
        .map_err(|e| EngineError::Other(format!("SendMouseInput: {e}")))
    }
}

fn wv2_move_along_path(
    comp: &ICoreWebView2CompositionController,
    start: vs_humanize::Point,
    end: vs_humanize::Point,
    mode: vs_humanize::InputMode,
    seed: u64,
) -> EngineResult<vs_humanize::Point> {
    let path = vs_humanize::mouse_path(start, end, mode, seed);
    let mut prev_ms: u128 = 0;
    for step in &path {
        if step.kind != vs_humanize::MouseStepKind::Move {
            continue;
        }
        wv2_send_mouse(comp, COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE, step.point)?;
        let dt = step.at.as_millis().saturating_sub(prev_ms);
        if dt > 0 {
            std::thread::sleep(Duration::from_millis(u64::try_from(dt).unwrap_or(0)));
        }
        prev_ms = step.at.as_millis();
    }
    // Final settling move ending exactly at `end`.
    wv2_send_mouse(comp, COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE, end)?;
    Ok(end)
}

fn wv2_press_release(
    comp: &ICoreWebView2CompositionController,
    at: vs_humanize::Point,
) -> EngineResult<()> {
    wv2_send_mouse(comp, COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN, at)?;
    std::thread::sleep(Duration::from_millis(15));
    wv2_send_mouse(comp, COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP, at)?;
    std::thread::sleep(Duration::from_millis(30));
    Ok(())
}

mod wv2_cookies {
    use std::sync::mpsc;

    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2Cookie, ICoreWebView2CookieList, ICoreWebView2CookieManager,
        ICoreWebView2_2, COREWEBVIEW2_COOKIE_SAME_SITE_KIND_LAX,
        COREWEBVIEW2_COOKIE_SAME_SITE_KIND_NONE, COREWEBVIEW2_COOKIE_SAME_SITE_KIND_STRICT,
    };
    use webview2_com::{pwstr_from_str, take_pwstr, GetCookiesCompletedHandler};
    use windows::core::{Interface, BOOL, HSTRING, PCWSTR, PWSTR};

    use crate::backend::auth::CookieData;
    use crate::engine::{EngineError, EngineResult};

    fn cookie_manager(web_view: &ICoreWebView2) -> EngineResult<ICoreWebView2CookieManager> {
        // CookieManager is on ICoreWebView2_2, not the base ICoreWebView2.
        // Cast (QueryInterface) then call.
        let v2: ICoreWebView2_2 = web_view
            .cast()
            .map_err(|e| EngineError::Other(format!("cast to ICoreWebView2_2: {e}")))?;
        unsafe { v2.CookieManager() }.map_err(|e| EngineError::Other(format!("CookieManager: {e}")))
    }

    pub(super) fn get_all_cookies(web_view: &ICoreWebView2) -> EngineResult<Vec<CookieData>> {
        let manager = cookie_manager(web_view)?;
        let (tx, rx) = mpsc::channel();
        let empty: HSTRING = HSTRING::new();
        let manager_for_init = manager.clone();
        GetCookiesCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| {
                unsafe { manager_for_init.GetCookies(PCWSTR(empty.as_ptr()), &handler) }
                    .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |hr, list| {
                hr?;
                let mut out: Vec<CookieData> = Vec::new();
                if let Some(list) = list {
                    if let Ok(count) = read_count(&list) {
                        for i in 0..count {
                            if let Ok(c) = unsafe { list.GetValueAtIndex(i) } {
                                out.push(serialize(&c));
                            }
                        }
                    }
                }
                let _ = tx.send(out);
                Ok(())
            }),
        )
        .map_err(|e| EngineError::Other(format!("GetCookies: {e:?}")))?;
        rx.recv()
            .map_err(|_| EngineError::Other("GetCookies: channel closed".into()))
    }

    pub(super) fn set_cookies(
        web_view: &ICoreWebView2,
        cookies: &[CookieData],
    ) -> EngineResult<()> {
        let manager = cookie_manager(web_view)?;
        for c in cookies {
            if c.name.is_empty() || c.domain.is_empty() {
                continue;
            }
            let name = pwstr_from_str(&c.name);
            let value = pwstr_from_str(&c.value);
            let domain = pwstr_from_str(&c.domain);
            let path_str = if c.path.is_empty() {
                "/"
            } else {
                c.path.as_str()
            };
            let path = pwstr_from_str(path_str);
            let cookie: ICoreWebView2Cookie = unsafe {
                manager.CreateCookie(
                    PCWSTR(name.0),
                    PCWSTR(value.0),
                    PCWSTR(domain.0),
                    PCWSTR(path.0),
                )
            }
            .map_err(|e| EngineError::Other(format!("CreateCookie: {e}")))?;
            unsafe { cookie.SetIsHttpOnly(c.http_only) }
                .map_err(|e| EngineError::Other(format!("SetIsHttpOnly: {e}")))?;
            unsafe { cookie.SetIsSecure(c.secure) }
                .map_err(|e| EngineError::Other(format!("SetIsSecure: {e}")))?;
            #[allow(clippy::cast_precision_loss)]
            if let Some(unix) = c.expires_unix {
                unsafe { cookie.SetExpires(unix as f64) }
                    .map_err(|e| EngineError::Other(format!("SetExpires: {e}")))?;
            }
            if let Some(ss) = c.same_site.as_deref() {
                let kind = match ss {
                    "Strict" => COREWEBVIEW2_COOKIE_SAME_SITE_KIND_STRICT,
                    "None" => COREWEBVIEW2_COOKIE_SAME_SITE_KIND_NONE,
                    _ => COREWEBVIEW2_COOKIE_SAME_SITE_KIND_LAX,
                };
                unsafe { cookie.SetSameSite(kind) }
                    .map_err(|e| EngineError::Other(format!("SetSameSite: {e}")))?;
            }
            unsafe { manager.AddOrUpdateCookie(&cookie) }
                .map_err(|e| EngineError::Other(format!("AddOrUpdateCookie: {e}")))?;
            // Free the PWSTR buffers we passed in (LocalAlloc'd by
            // pwstr_from_str; take_pwstr frees on drop).
            let _ = take_pwstr(name);
            let _ = take_pwstr(value);
            let _ = take_pwstr(domain);
            let _ = take_pwstr(path);
        }
        Ok(())
    }

    fn read_count(list: &ICoreWebView2CookieList) -> windows::core::Result<u32> {
        let mut count: u32 = 0;
        unsafe { list.Count(&raw mut count) }?;
        Ok(count)
    }

    fn read_pwstr_getter<F>(call: F) -> String
    where
        F: FnOnce(&mut PWSTR) -> windows::core::Result<()>,
    {
        let mut out: PWSTR = PWSTR::null();
        match call(&mut out) {
            Ok(()) if !out.is_null() => take_pwstr(out),
            _ => String::new(),
        }
    }

    fn read_bool_getter<F>(call: F) -> bool
    where
        F: FnOnce(&mut BOOL) -> windows::core::Result<()>,
    {
        let mut out: BOOL = BOOL(0);
        match call(&mut out) {
            Ok(()) => out.as_bool(),
            Err(_) => false,
        }
    }

    fn serialize(c: &ICoreWebView2Cookie) -> CookieData {
        let name = read_pwstr_getter(|p| unsafe { c.Name(&raw mut *p) });
        let value = read_pwstr_getter(|p| unsafe { c.Value(&raw mut *p) });
        let domain = read_pwstr_getter(|p| unsafe { c.Domain(&raw mut *p) });
        let path = read_pwstr_getter(|p| unsafe { c.Path(&raw mut *p) });
        let secure = read_bool_getter(|b| unsafe { c.IsSecure(&raw mut *b) });
        let http_only = read_bool_getter(|b| unsafe { c.IsHttpOnly(&raw mut *b) });
        let expires_unix = {
            let mut d: f64 = 0.0;
            match unsafe { c.Expires(&raw mut d) } {
                Ok(()) if d > 0.0 => {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                    let i = d.round() as i64;
                    Some(i)
                }
                _ => None,
            }
        };
        let same_site = {
            let mut k = COREWEBVIEW2_COOKIE_SAME_SITE_KIND_LAX;
            match unsafe { c.SameSite(&raw mut k) } {
                Ok(()) => match k {
                    COREWEBVIEW2_COOKIE_SAME_SITE_KIND_STRICT => Some("Strict".to_string()),
                    COREWEBVIEW2_COOKIE_SAME_SITE_KIND_NONE => Some("None".to_string()),
                    COREWEBVIEW2_COOKIE_SAME_SITE_KIND_LAX => Some("Lax".to_string()),
                    _ => None,
                },
                _ => None,
            }
        };
        CookieData {
            name,
            value,
            domain,
            path,
            expires_unix,
            secure,
            http_only,
            same_site,
        }
    }
}
