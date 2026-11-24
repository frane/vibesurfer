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

use super::native_download::{name_from_disposition, temp_path, SharedDownloads, MAX_BYTES};

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2, ICoreWebView2CompositionController, ICoreWebView2Controller,
    ICoreWebView2ControllerOptions, ICoreWebView2Environment, ICoreWebView2Environment10,
    ICoreWebView2Environment3, COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
    COREWEBVIEW2_MOUSE_EVENT_KIND, COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL,
    COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN, COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP,
    COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE, COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL,
    COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_NONE, COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC,
};
use webview2_com::{
    pwstr_from_str, take_pwstr, AddScriptToExecuteOnDocumentCreatedCompletedHandler,
    CallDevToolsProtocolMethodCompletedHandler, CapturePreviewCompletedHandler,
    CreateCoreWebView2CompositionControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, ExecuteScriptCompletedHandler,
    FrameCreatedEventHandler, NavigationCompletedEventHandler, NewWindowRequestedEventHandler,
    ScriptDialogOpeningEventHandler, WebMessageReceivedEventHandler,
    WindowCloseRequestedEventHandler,
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
    ActTarget, Action, AuthBlob, CaptureScope, CursorOp, DataStore, Download, DownloadEntry,
    DownloadSource, Engine, EngineCapabilities, EngineError, EngineResult, InputMode, LayoutBox,
    PageHandle, Viewport, WaitCondition,
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
    /// URL of a new-window navigation (`target="_blank"`,
    /// `window.open()`) the page asked for and we have not loaded yet.
    /// Set by the `NewWindowRequested` handler installed in `open`.
    new_window: std::rc::Rc<std::cell::RefCell<Option<String>>>,
    /// Downloads this page's responses turned into.
    downloads: SharedDownloads,
    /// Main-document response of the latest navigation, assembled by
    /// the handlers from `install_response_capture`.
    responses: std::rc::Rc<std::cell::RefCell<ResponseCapture>>,
    /// The page's session asked for real popups (`Engine::set_popups`):
    /// a new window becomes a page, `window.close()` releases it, and
    /// dialogs wait for an answer. Read by the view's event handlers.
    popups_on: std::rc::Rc<std::cell::Cell<bool>>,
    /// The page's own handle, for those handlers (set once allocated).
    me: std::rc::Rc<std::cell::Cell<Option<PageHandle>>>,
    /// Subframes, for evaluating in one (`webview2_extras`).
    frames: super::webview2_extras::Frames,
}

/// What a popup's view needs from its opener: the same environment and
/// profile, and whether the session is proxied.
#[derive(Clone)]
struct ViewOrigin {
    environment: ICoreWebView2Environment,
    profile_key: Option<String>,
    proxied: bool,
}

type Popups = std::rc::Rc<std::cell::RefCell<super::webview2_extras::PopupState<W2Page>>>;

/// What `install_response_capture` has seen of the latest navigation.
/// WebView2 reports the status on `NavigationCompleted` and headers on
/// `WebResourceResponseReceived`, which has no frame or resource-type
/// information, so responses are kept by URL while the navigation is in
/// flight and matched against the final `Source` when it completes.
#[derive(Default)]
struct ResponseCapture {
    in_flight: bool,
    responses: Vec<crate::inspector::MainResponse>,
    /// Final URL and HTTP status of the completed navigation.
    done: Option<(String, u16)>,
}

impl ResponseCapture {
    /// Responses kept per navigation; the main document's comes early.
    const MAX: usize = 64;

    fn main_response(&self) -> Option<crate::inspector::MainResponse> {
        let (url, status) = self.done.clone()?;
        let found = self.responses.iter().rev().find(|r| r.url == url).cloned();
        Some(found.unwrap_or_else(|| crate::inspector::MainResponse {
            url,
            status,
            status_text: crate::inspector::reason_phrase(status).to_string(),
            headers: Vec::new(),
        }))
    }
}

// =============================================================================
// Backend
// =============================================================================

/// WebView2 backend. Construct after `CoInitializeEx` on the message
/// thread; subsequent calls assume that same thread.
pub struct Webview2Backend {
    pages: HashMap<PageHandle, W2Page>,
    /// Popups and closed pages from the views' event handlers, the
    /// popup feed, and the page-handle counter.
    popups: Popups,
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
    /// Drop a new-window request parked before this input primitive
    /// started (a popup the page fired on its own), so the primitive
    /// only follows what it caused.
    fn forget_new_window(&mut self, page: PageHandle) -> EngineResult<()> {
        let _ = self.page_mut(page)?.new_window.borrow_mut().take();
        let mut s = self.popups.borrow_mut();
        s.raised_at_input = s.feed.raised();
        Ok(())
    }

    /// Navigate this page to a new-window URL the last input primitive
    /// caused, and wait for it like `goto`. `NewWindowRequested` is
    /// raised asynchronously, so after a click (`settle`) pump messages
    /// for up to `NEW_WINDOW_SETTLE` first. No request: no-op.
    fn follow_new_window(&mut self, page: PageHandle, settle: bool) -> EngineResult<()> {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, MSG, PM_REMOVE,
        };
        let (parked, web_view, popups_on) = {
            let p = self.page_mut(page)?;
            (p.new_window.clone(), p.web_view.clone(), p.popups_on.get())
        };
        let wait = settle
            && parked.borrow().is_none()
            && super::common::new_window_hinted(&execute_script(
                &web_view,
                super::common::NEW_WINDOW_HINT_JS,
            ));
        let deadline = std::time::Instant::now()
            + if wait {
                super::common::NEW_WINDOW_SETTLE
            } else {
                Duration::ZERO
            };
        // A real window: wait for WebView2 to report it, so the popup
        // exists when the click returns.
        let (feed, before) = {
            let s = self.popups.borrow();
            (s.feed.clone(), s.raised_at_input)
        };
        let arrived = || {
            if popups_on {
                feed.raised() > before
            } else {
                parked.borrow().is_some()
            }
        };
        while !arrived() && std::time::Instant::now() < deadline {
            let mut msg = MSG::default();
            unsafe {
                while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&raw const msg);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let Some(url) = parked.borrow_mut().take() else {
            return Ok(());
        };
        self.navigate(page, &url)
    }

    /// Build a backend. Caller must already have initialized COM
    /// (`CoInitializeEx` STA) on this thread before constructing.
    /// Production wiring lives in `vs-cli::serve` on Windows.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pages: HashMap::new(),
            popups: Popups::default(),
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
        self.popups.borrow_mut().alloc()
    }

    /// Take in the windows pages opened and release the pages that
    /// closed themselves since the last call. Runs before any page
    /// lookup, so a popup's handle works as soon as it is reported.
    fn sync_popups(&mut self) {
        let (born, closed) = {
            let mut s = self.popups.borrow_mut();
            (std::mem::take(&mut s.born), std::mem::take(&mut s.closed))
        };
        for b in born {
            self.pages.insert(b.handle, b.page);
        }
        for h in closed {
            let _ = self.close(h);
        }
    }

    fn page_mut(&mut self, h: PageHandle) -> EngineResult<&mut W2Page> {
        self.sync_popups();
        self.pages.get_mut(&h).ok_or(EngineError::NotFound {
            kind: "page",
            id: h.0.to_string(),
        })
    }

    /// A host HWND for a new page (see `new_host_hwnd`).
    fn create_host_hwnd(&mut self) -> EngineResult<HWND> {
        let hwnd = new_host_hwnd()?;
        // Remember the most recent HWND only so `Default::default()`
        // is satisfied; per-page HWNDs live on `W2Page::parent_hwnd`.
        self.parent_hwnd = Some(hwnd);
        Ok(hwnd)
    }
}

/// Create a fresh message-only HWND to host a page's WebView2
/// composition target. DirectComposition rejects a second
/// `CreateTargetForHwnd` on a window that already has a target
/// (`DCOMPOSITION_ERROR_WINDOW_ALREADY_COMPOSED`), so every page gets its
/// own HWND under the same shared class.
fn new_host_hwnd() -> EngineResult<HWND> {
    {
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

/// Call a DevTools Protocol method on `web_view` and return its JSON
/// result.
///
/// This is how keyboard input reaches the page on Windows. WebView2's
/// visual hosting has `SendMouseInput` but no keyboard equivalent —
/// in that mode the host is expected to forward Win32 key messages,
/// which a headless daemon with no focused window cannot do. The
/// DevTools protocol dispatches straight into the renderer instead,
/// producing `isTrusted=true` key events without needing window
/// focus, which is what the primitive is for.
fn call_cdp(web_view: &ICoreWebView2, method: &str, params_json: &str) -> EngineResult<String> {
    let (tx, rx) = mpsc::channel();
    let method_owned = method.to_string();
    let params_owned = params_json.to_string();
    let web_view_owned: ICoreWebView2 = web_view.clone();
    CallDevToolsProtocolMethodCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            let m = pwstr_from_str(&method_owned);
            let p = pwstr_from_str(&params_owned);
            unsafe {
                web_view_owned.CallDevToolsProtocolMethod(
                    windows::core::PCWSTR(m.0),
                    windows::core::PCWSTR(p.0),
                    &handler,
                )
            }
            .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |error_code, result_json| {
            error_code?;
            let _ = tx.send(result_json);
            Ok(())
        }),
    )
    .map_err(|e| EngineError::Other(format!("CallDevToolsProtocolMethod({method}): {e:?}")))?;
    rx.recv()
        .map_err(|_| EngineError::Other("CallDevToolsProtocolMethod: channel closed".into()))
}

/// A click with another button, count or modifiers, as DevTools input
/// (trusted, like `SendMouseInput`, which has no Alt or Meta): one
/// pressed/released pair per click, with `clickCount` rising, so the
/// page sees `dblclick` and `detail`.
fn cdp_click(
    web_view: &ICoreWebView2,
    at: vs_humanize::Point,
    opts: crate::engine::ClickOpts,
) -> EngineResult<()> {
    use crate::engine::MouseButton;
    let button = match opts.button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    };
    let m = opts.modifiers;
    // Alt=1, Ctrl=2, Meta=4, Shift=8.
    let modifiers = u8::from(m.alt)
        | (u8::from(m.ctrl) << 1)
        | (u8::from(m.meta) << 2)
        | (u8::from(m.shift) << 3);
    for n in 1..=opts.count.max(1) {
        for ty in ["mousePressed", "mouseReleased"] {
            let params = serde_json::json!({
                "type": ty, "x": at.x, "y": at.y, "button": button,
                "clickCount": n, "modifiers": modifiers,
            });
            call_cdp(web_view, "Input.dispatchMouseEvent", &params.to_string())?;
        }
    }
    Ok(())
}

/// How one character is spelled to `Input.dispatchKeyEvent`.
///
/// A printable character identifies itself and is inserted because of
/// `text`. A named key must not carry text, or the renderer inserts a
/// character instead of acting on the key; Enter is the exception,
/// where the text is what submits a form.
struct CdpKey {
    key: String,
    code: Option<&'static str>,
    virtual_key: u32,
    text: String,
}

fn cdp_key(ch: char) -> CdpKey {
    let named = match ch {
        '\n' | '\r' => Some(("Enter", "Enter", 13_u32, "\r")),
        '\t' => Some(("Tab", "Tab", 9, "")),
        '\u{8}' => Some(("Backspace", "Backspace", 8, "")),
        '\u{1b}' => Some(("Escape", "Escape", 27, "")),
        _ => None,
    };
    match named {
        Some((key, code, virtual_key, text)) => CdpKey {
            key: key.to_string(),
            code: Some(code),
            virtual_key,
            text: text.to_string(),
        },
        None => CdpKey {
            key: ch.to_string(),
            code: None,
            virtual_key: 0,
            text: ch.to_string(),
        },
    }
}

thread_local! {
    /// Set while a popup's view is set up inside WebView2's
    /// controller-created callback (`spawn_popup`), where waiting for an
    /// async completion pumps messages inside the callback and never
    /// returns. Scripts are then registered without waiting: the call
    /// takes effect at once, so the popup's first document gets them.
    static NO_WAIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with `add_init_script` not waiting for completions.
fn without_waiting<T>(f: impl FnOnce() -> T) -> T {
    NO_WAIT.with(|w| w.set(true));
    let out = f();
    NO_WAIT.with(|w| w.set(false));
    out
}

/// Add `js` to the document-start initializer for `web_view`.
/// Returns the script id, which the caller stores in case we want to
/// remove it later (empty when not waited for; see `NO_WAIT`).
fn add_init_script(web_view: &ICoreWebView2, js: &str) -> EngineResult<String> {
    if NO_WAIT.with(std::cell::Cell::get) {
        let done =
            AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(|_, _| Ok(())));
        let w = pwstr_from_str(js);
        unsafe { web_view.AddScriptToExecuteOnDocumentCreated(windows::core::PCWSTR(w.0), &done) }
            .map_err(|e| EngineError::Other(format!("AddScriptToExecuteOnDocumentCreated: {e}")))?;
        return Ok(String::new());
    }
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
    // Wrapped in an IIFE: a top-level `function __vsMakeHandler` here
    // became a global, and `Object.keys(window)` then listed it
    // alongside the page's own properties — the same instrumentation
    // leak the shared shims were fixed for. Caught by the
    // fingerprint cell on windows-latest.
    let shim = r"
      (function () {
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
      })();
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
    unsafe { web_view.add_WebMessageReceived(&handler, &raw mut token) }.is_ok()
}

// =============================================================================
// Engine impl
// =============================================================================

/// Controller options for an isolated session: InPrivate, with a
/// profile name derived from `key` so the session's pages share one
/// profile and no other session's pages do. Profile names allow only a
/// small character set, so anything else in the key becomes `_`.
fn isolated_controller_options(
    environment: &ICoreWebView2Environment,
    key: &str,
) -> EngineResult<(ICoreWebView2Environment10, ICoreWebView2ControllerOptions)> {
    let env10: ICoreWebView2Environment10 = environment.cast().map_err(|e| {
        EngineError::Other(format!(
            "isolated sessions need WebView2 runtime with profiles: {e}"
        ))
    })?;
    let options = unsafe { env10.CreateCoreWebView2ControllerOptions() }
        .map_err(|e| EngineError::Other(format!("CreateControllerOptions: {e}")))?;
    let name: String = std::iter::once("vsiso_")
        .chain(std::iter::once(key))
        .flat_map(str::chars)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    unsafe {
        options
            .SetProfileName(&HSTRING::from(name))
            .map_err(|e| EngineError::Other(format!("SetProfileName: {e}")))?;
        options
            .SetIsInPrivateModeEnabled(true)
            .map_err(|e| EngineError::Other(format!("SetIsInPrivateModeEnabled: {e}")))?;
    }
    Ok((env10, options))
}

/// Allow certificate errors in an intercept page (see `open`).
fn allow_proxy_certificates(web_view: &ICoreWebView2) -> EngineResult<()> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_14, COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_ALWAYS_ALLOW,
    };
    let wv14: ICoreWebView2_14 = web_view
        .cast()
        .map_err(|e| EngineError::Other(format!("cast ICoreWebView2_14: {e}")))?;
    let handler = webview2_com::ServerCertificateErrorDetectedEventHandler::create(Box::new(
        |_sender, args| {
            if let Some(args) = args {
                unsafe {
                    args.SetAction(COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_ALWAYS_ALLOW)
                }?;
            }
            Ok(())
        },
    ));
    let mut token: i64 = 0;
    unsafe { wv14.add_ServerCertificateErrorDetected(&handler, &raw mut token) }
        .map_err(|e| EngineError::Other(format!("add_ServerCertificateErrorDetected: {e}")))
}

/// User data folder for a proxied session's own browser process.
fn proxied_user_data_dir(key: &str) -> std::path::PathBuf {
    let name: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    std::env::temp_dir().join(format!("vibesurfer-intercept-{name}"))
}

/// Make `comp_controller` (hosted on `parent`) a page: DirectComposition
/// wiring, bounds, visibility and focus, the inspector bridge, download
/// and response capture, and the new-window, close, dialog and frame
/// handlers. Used for pages `open` creates and for windows a page opens,
/// so a popup is the same kind of page. The caller sets `me` once the
/// page has a handle.
#[allow(clippy::too_many_lines)]
fn host_view(
    origin: &ViewOrigin,
    parent: HWND,
    comp_controller: ICoreWebView2CompositionController,
    popups: &Popups,
) -> EngineResult<W2Page> {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_4;

    let controller: ICoreWebView2Controller = comp_controller
        .cast::<ICoreWebView2Controller>()
        .map_err(|e| EngineError::Other(format!("cast Controller: {e}")))?;

    // DirectComposition wiring. CompositionController needs a root
    // visual to render into: one bound to the hidden parent HWND, handed
    // to the controller and committed once. The device, target and
    // visual are stored on the page so their refcounts outlive this call.
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
    // `IsVisible` defaults to FALSE, which suspends rendering, and after
    // a `viewport`/`SetBounds` an invisible controller never re-renders,
    // so the next `CapturePreview` never completes. Visible + offscreen
    // composition = continuous frames, nothing shown on screen.
    unsafe { controller.SetIsVisible(true) }
        .map_err(|e| EngineError::Other(format!("SetIsVisible: {e}")))?;
    // Give the page focus. Without it `document.hasFocus()` is false and
    // the page behaves as a backgrounded tab (`autofocus`,
    // `:focus-visible`, IME). Not fatal if it fails.
    if let Err(e) = unsafe { controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC) } {
        eprintln!("vs: MoveFocus failed ({e:?}); document.hasFocus() will be false");
    }

    let web_view: ICoreWebView2 = unsafe { controller.CoreWebView2() }
        .map_err(|e| EngineError::Other(format!("CoreWebView2: {e}")))?;

    // Deliberately no User-Agent override: WebView2 is Chromium, and a
    // UA that disagrees with the engine underneath is the loudest
    // inconsistency a fingerprinter can find.

    // Inspector bridge before any navigation, so the document-start hook
    // fires on the loaded page. Download capture, same shim as macOS and
    // Linux (`AddScriptToExecuteOnDocumentCreated` covers every frame).
    let inspector = InspectorSlots::new(crate::inspector::DEFAULT_BUFFER_CAPACITY);
    let inspector_installed = install_inspector(&web_view, &inspector);
    let _ = add_init_script(&web_view, super::common::DOWNLOAD_SHIM_JS);
    // No native drags (see NO_NATIVE_DRAG_JS).
    let _ = add_init_script(&web_view, super::common::NO_NATIVE_DRAG_JS);
    let downloads = SharedDownloads::default();
    install_download_capture(&web_view, &downloads)?;
    let responses = install_response_capture(&web_view)?;

    // Dialogs go through `ScriptDialogOpening`: held for an answer in a
    // page with popups on, dismissed at once otherwise (as on macOS).
    if let Ok(settings) = unsafe { web_view.Settings() } {
        let _ = unsafe { settings.SetAreDefaultScriptDialogsEnabled(false) };
        // No native context menu: the page still gets its contextmenu
        // event, and nothing stays open to swallow the next click.
        let _ = unsafe { settings.SetAreDefaultContextMenusEnabled(false) };
        // No SmartScreen lookups: they are Edge's own requests, which
        // otherwise show up among the page's (intercept sessions) and
        // cost time on every navigation.
        if let Ok(s8) =
            settings.cast::<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings8>()
        {
            let _ = unsafe { s8.SetIsReputationCheckingRequired(false) };
        }
    }

    let popups_on = Rc::new(Cell::new(false));
    let me: Rc<Cell<Option<PageHandle>>> = Rc::new(Cell::new(None));
    let frames: super::webview2_extras::Frames = Rc::default();

    // A new window. With popups on it becomes a page of its own in the
    // same environment and profile (`spawn_popup`). Otherwise there is
    // no second window: mark the request handled and park its URL, and
    // the input primitive that caused it navigates this page there
    // (`follow_new_window`). WebView2 does not expose the request body,
    // so a POSTed blank-target form arrives as a GET.
    let new_window: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    {
        let parked = new_window.clone();
        let on = popups_on.clone();
        let me = me.clone();
        let popups = popups.clone();
        let origin = origin.clone();
        let handler = NewWindowRequestedEventHandler::create(Box::new(move |_sender, args| {
            let Some(args) = args else { return Ok(()) };
            let mut uri = PWSTR(std::ptr::null_mut());
            let url = if unsafe { args.Uri(&raw mut uri) }.is_ok() {
                take_pwstr(uri)
            } else {
                String::new()
            };
            if on.get() {
                spawn_popup(&origin, &args, me.get(), url, &popups);
                return Ok(());
            }
            *parked.borrow_mut() = Some(url);
            unsafe { args.SetHandled(true) }?;
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe { web_view.add_NewWindowRequested(&handler, &raw mut token) }
            .map_err(|e| EngineError::Other(format!("add_NewWindowRequested: {e}")))?;
    }
    // `window.close()`: with popups on, report it; the backend releases
    // the page on its next lookup.
    {
        let on = popups_on.clone();
        let me = me.clone();
        let popups = popups.clone();
        let handler = WindowCloseRequestedEventHandler::create(Box::new(move |_, _| {
            if let (true, Some(h)) = (on.get(), me.get()) {
                let mut s = popups.borrow_mut();
                s.closed.push(h);
                s.feed.push(crate::engine::PopupEvent::Closed(h));
            }
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe { web_view.add_WindowCloseRequested(&handler, &raw mut token) }
            .map_err(|e| EngineError::Other(format!("add_WindowCloseRequested: {e}")))?;
    }
    // `alert`/`confirm`/`prompt`.
    {
        let on = popups_on.clone();
        let me = me.clone();
        let popups = popups.clone();
        let handler = ScriptDialogOpeningEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else { return Ok(()) };
            if let (true, Some(h)) = (on.get(), me.get()) {
                let feed = popups.borrow().feed.clone();
                super::webview2_extras::hold_dialog(&feed, h, &args)?;
            }
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe { web_view.add_ScriptDialogOpening(&handler, &raw mut token) }
            .map_err(|e| EngineError::Other(format!("add_ScriptDialogOpening: {e}")))?;
    }
    // Subframes, for `eval_js_in_frame`.
    if let Ok(wv4) = web_view.cast::<ICoreWebView2_4>() {
        let sink = frames.clone();
        let handler = FrameCreatedEventHandler::create(Box::new(move |_, args| {
            if let Some(args) = args {
                let frame = unsafe { args.Frame() }?;
                super::webview2_extras::track_frame(&sink, &frame)?;
            }
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe { wv4.add_FrameCreated(&handler, &raw mut token) }
            .map_err(|e| EngineError::Other(format!("add_FrameCreated: {e}")))?;
    }

    // An intercept page's TLS ends at the daemon's proxy, which verifies
    // upstream itself. Should Chromium still report a certificate error
    // for the proxy's leaves, allow it: only this session's pages get here.
    if origin.proxied {
        allow_proxy_certificates(&web_view)?;
    }

    Ok(W2Page {
        comp_controller,
        controller,
        web_view,
        parent_hwnd: parent,
        _dcomp_device: dcomp_device,
        _dcomp_target: dcomp_target,
        _dcomp_visual: dcomp_visual,
        inspector,
        inspector_installed,
        cookie_baseline: RefCell::new(None),
        cookie_next_seq: RefCell::new(0),
        last_mouse: Cell::new(vs_humanize::Point { x: 0.0, y: 0.0 }),
        new_window,
        downloads,
        responses,
        popups_on,
        me,
        frames,
    })
}

/// A window `opener` asked for (`NewWindowRequested`), as a page of its
/// own: a composition controller in the opener's environment and profile,
/// handed to WebView2 as the new window (so `window.opener` works) and
/// reported on the feed. The controller is created asynchronously, under
/// a deferral of the request.
fn spawn_popup(
    origin: &ViewOrigin,
    args: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2NewWindowRequestedEventArgs,
    opener: Option<PageHandle>,
    url: String,
    popups: &Popups,
) {
    let Ok(deferral) = (unsafe { args.GetDeferral() }) else {
        return;
    };
    let finish = {
        let deferral = deferral.clone();
        move || {
            let _ = unsafe { deferral.Complete() };
        }
    };
    let (Ok(parent), Ok(env3)) = (
        new_host_hwnd(),
        origin.environment.cast::<ICoreWebView2Environment3>(),
    ) else {
        finish();
        return;
    };
    let Ok(iso) = origin
        .profile_key
        .as_deref()
        .map(|key| isolated_controller_options(&origin.environment, key))
        .transpose()
    else {
        finish();
        return;
    };
    let (origin, args, popups) = (origin.clone(), args.clone(), popups.clone());
    let done = CreateCoreWebView2CompositionControllerCompletedHandler::create(Box::new(
        move |error_code, controller| {
            let adopted = (|| -> EngineResult<()> {
                error_code.map_err(|e| EngineError::Other(format!("popup controller: {e}")))?;
                let comp = controller
                    .ok_or_else(|| EngineError::Other("popup controller: none".into()))?;
                let page = without_waiting(|| host_view(&origin, parent, comp, &popups))?;
                page.popups_on.set(true);
                unsafe { args.SetNewWindow(&page.web_view) }
                    .map_err(|e| EngineError::Other(format!("SetNewWindow: {e}")))?;
                let mut s = popups.borrow_mut();
                let handle = s.alloc();
                page.me.set(Some(handle));
                s.born.push(super::webview2_extras::Born { handle, page });
                s.feed
                    .push(crate::engine::PopupEvent::Opened(crate::engine::Popup {
                        opener,
                        page: handle,
                        url: url.clone(),
                    }));
                Ok(())
            })();
            if let Err(e) = adopted {
                eprintln!("vs: popup failed: {e}");
            }
            finish();
            Ok(())
        },
    ));
    let started = unsafe {
        match iso {
            None => env3.CreateCoreWebView2CompositionController(parent, &done),
            Some((env10, options)) => {
                env10.CreateCoreWebView2CompositionControllerWithOptions(parent, &options, &done)
            }
        }
    };
    if started.is_err() {
        let _ = unsafe { deferral.Complete() };
    }
}

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
        self.forget_new_window(page)?;
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        super::common::run_act(
            move |js, _budget| execute_script(&web_view, js),
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
        let native = p.downloads.clone();
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
            &native,
        )
    }

    fn download_list(&mut self, page: PageHandle) -> EngineResult<Vec<DownloadEntry>> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        let native = p.downloads.clone();
        super::common::run_download_list(move |js, _budget| execute_script(&web_view, js), &native)
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
            crate::engine::CookieOp::Set(list) => wv2_cookies::set_cookies(&web_view, &list)?,
            crate::engine::CookieOp::Clear => wv2_cookies::delete_all(&web_view)?,
        }
        wv2_cookies::get_all_cookies(&web_view)
    }

    fn save_auth(&mut self, page: PageHandle) -> EngineResult<AuthBlob> {
        let p = self.page_mut(page)?;
        let web_view = p.web_view.clone();
        // Cookies: host-side via ICoreWebView2CookieManager so
        // HttpOnly entries are captured. localStorage/sessionStorage:
        // JS shim.
        let cookies = wv2_cookies::get_all_cookies(&web_view)?;
        let storage = super::common::run_save_storage_only(
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
        wv2_cookies::set_cookies(&web_view, &parsed.cookies)?;
        super::common::run_load_storage_only(
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
        super::common::run_eval(move |js, _budget| execute_script(&web_view, js), expr)
    }

    fn eval_js_in_frame(
        &mut self,
        page: PageHandle,
        frame: &str,
        expr: &str,
    ) -> EngineResult<crate::inspector::EvalResult> {
        let frames = self.page_mut(page)?.frames.clone();
        let target = super::webview2_extras::find_frame(&frames, frame).ok_or_else(|| {
            EngineError::NotFound {
                kind: "frame",
                id: frame.to_string(),
            }
        })?;
        super::common::run_eval(
            move |js, _budget| super::webview2_extras::execute_in_frame(&target, js),
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
        let current = wv2_cookies::get_all_cookies(&web_view)?;
        let previous = p.cookie_baseline.borrow().clone();
        let mut seq = p.cookie_next_seq.borrow_mut();
        let events = super::common::diff_cookies(previous.as_deref(), &current, &mut seq);
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
                let html5_js = super::common::build_html5_drag_js(x1, y1, x2, y2);
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
        CursorOp::Wheel { x, y, dx, dy } => (x, y, dx, dy),
        CursorOp::Click { x, y, .. } => (x, y, 0.0, 0.0),
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

/// A trusted wheel turn of `(dx, dy)` CSS px at `point`. `mouseData`
/// is in 120ths of a notch, positive away from the user (up) for the
/// vertical wheel and to the right for the horizontal one; Chromium
/// scrolls a notch as 100 px, so one CSS px is 1.2 units.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn wv2_wheel(
    comp: &ICoreWebView2CompositionController,
    point: vs_humanize::Point,
    dx: f64,
    dy: f64,
) -> EngineResult<()> {
    let units = |px: f64| (px * 1.2).round() as i32;
    for (kind, data) in [
        (COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL, -units(dy)),
        (COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL, units(dx)),
    ] {
        if data == 0 {
            continue;
        }
        unsafe {
            comp.SendMouseInput(
                kind,
                COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_NONE,
                data as u32,
                point_at(point),
            )
            .map_err(|e| EngineError::Other(format!("SendMouseInput wheel: {e}")))?;
        }
    }
    Ok(())
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

    /// Delete every cookie of the web view's profile.
    pub(super) fn delete_all(web_view: &ICoreWebView2) -> EngineResult<()> {
        let manager = cookie_manager(web_view)?;
        unsafe { manager.DeleteAllCookies() }
            .map_err(|e| EngineError::Other(format!("DeleteAllCookies: {e}")))
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

/// Read a `PWSTR` out-parameter; empty on failure or null.
fn out_pwstr(f: impl FnOnce(*mut PWSTR) -> windows::core::Result<()>) -> String {
    let mut p = PWSTR(std::ptr::null_mut());
    if f(&raw mut p).is_ok() && !p.0.is_null() {
        take_pwstr(p)
    } else {
        String::new()
    }
}

/// Track the main document's response for `Engine::main_response`.
///
/// `NavigationStarting` (main frame only) clears the record, response
/// events are kept by URL until `NavigationCompleted`, and completion
/// stores the final `Source` and `HttpStatusCode`. A status of 0 (a
/// failed load, `data:`, `about:blank`) or a non-HTTP source records
/// nothing.
fn install_response_capture(
    web_view: &ICoreWebView2,
) -> EngineResult<std::rc::Rc<std::cell::RefCell<ResponseCapture>>> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2NavigationCompletedEventArgs2, ICoreWebView2_2,
    };
    use webview2_com::{NavigationStartingEventHandler, WebResourceResponseReceivedEventHandler};
    use windows::core::BOOL;

    let state = std::rc::Rc::new(std::cell::RefCell::new(ResponseCapture::default()));

    let s = state.clone();
    let starting = NavigationStartingEventHandler::create(Box::new(move |_sender, _args| {
        let mut st = s.borrow_mut();
        st.in_flight = true;
        st.responses.clear();
        st.done = None;
        Ok(())
    }));
    let mut token: i64 = 0;
    unsafe { web_view.add_NavigationStarting(&starting, &raw mut token) }
        .map_err(|e| EngineError::Other(format!("add_NavigationStarting: {e}")))?;

    let s = state.clone();
    let completed = NavigationCompletedEventHandler::create(Box::new(move |sender, args| {
        let mut st = s.borrow_mut();
        st.in_flight = false;
        let mut status: i32 = 0;
        if let Some(a2) =
            args.and_then(|a| a.cast::<ICoreWebView2NavigationCompletedEventArgs2>().ok())
        {
            let _ = unsafe { a2.HttpStatusCode(&raw mut status) };
        }
        let source = sender
            .map(|v| out_pwstr(|p| unsafe { v.Source(p) }))
            .unwrap_or_default();
        let http = source.starts_with("http:") || source.starts_with("https:");
        st.done = match u16::try_from(status) {
            Ok(code) if code > 0 && http => Some((source.clone(), code)),
            _ => None,
        };
        st.responses.retain(|r| r.url == source);
        Ok(())
    }));
    let mut token: i64 = 0;
    unsafe { web_view.add_NavigationCompleted(&completed, &raw mut token) }
        .map_err(|e| EngineError::Other(format!("add_NavigationCompleted: {e}")))?;

    let v2: ICoreWebView2_2 = web_view
        .cast()
        .map_err(|e| EngineError::Other(format!("cast to ICoreWebView2_2: {e}")))?;
    let s = state.clone();
    let received =
        WebResourceResponseReceivedEventHandler::create(Box::new(move |_sender, args| {
            let Some(args) = args else { return Ok(()) };
            let uri = out_pwstr(|p| unsafe { args.Request()?.Uri(p) });
            {
                let st = s.borrow();
                // While the navigation is in flight, keep everything up to
                // the cap. After it, only a late response for the document.
                let wanted = if st.in_flight {
                    st.responses.len() < ResponseCapture::MAX
                } else {
                    st.done.as_ref().is_some_and(|(u, _)| *u == uri)
                        && !st.responses.iter().any(|r| r.url == uri)
                };
                if !wanted {
                    return Ok(());
                }
            }
            let resp = unsafe { args.Response() }?;
            let mut status: i32 = 0;
            unsafe { resp.StatusCode(&raw mut status) }?;
            let status = u16::try_from(status).unwrap_or(0);
            let mut status_text = out_pwstr(|p| unsafe { resp.ReasonPhrase(p) });
            if status_text.is_empty() {
                status_text = crate::inspector::reason_phrase(status).to_string();
            }
            let mut headers = Vec::new();
            let iter = unsafe { resp.Headers()?.GetIterator() }?;
            loop {
                let mut has = BOOL::default();
                unsafe { iter.HasCurrentHeader(&raw mut has) }?;
                if !has.as_bool() {
                    break;
                }
                let mut name = PWSTR(std::ptr::null_mut());
                let mut value = PWSTR(std::ptr::null_mut());
                unsafe { iter.GetCurrentHeader(&raw mut name, &raw mut value) }?;
                headers.push((take_pwstr(name).to_ascii_lowercase(), take_pwstr(value)));
                let mut next = BOOL::default();
                unsafe { iter.MoveNext(&raw mut next) }?;
            }
            s.borrow_mut()
                .responses
                .push(crate::inspector::MainResponse {
                    url: uri,
                    status,
                    status_text,
                    headers,
                });
            Ok(())
        }));
    let mut token: i64 = 0;
    unsafe { v2.add_WebResourceResponseReceived(&received, &raw mut token) }
        .map_err(|e| EngineError::Other(format!("add_WebResourceResponseReceived: {e}")))?;

    Ok(state)
}

/// Take over WebView2's own downloads.
///
/// WebView2 turns a `Content-Disposition: attachment` response (an
/// export button, a form POST answering with a file, a hidden-iframe
/// download) into a download with a shelf UI and a file in the user's
/// Downloads folder. A headless page wants neither: write it to a temp
/// file, hide the UI, and move the bytes into `downloads` for
/// `vs_download`. `blob:` and `data:` saves come from page script and
/// the shim already has them, so those are cancelled here.
fn install_download_capture(
    web_view: &ICoreWebView2,
    downloads: &SharedDownloads,
) -> EngineResult<()> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2DownloadOperation, ICoreWebView2_4, COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON,
        COREWEBVIEW2_DOWNLOAD_STATE, COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED,
        COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED,
    };
    use webview2_com::{DownloadStartingEventHandler, StateChangedEventHandler};

    fn read_pwstr(f: impl FnOnce(*mut PWSTR) -> windows::core::Result<()>) -> String {
        let mut p = PWSTR(std::ptr::null_mut());
        if f(&raw mut p).is_ok() && !p.0.is_null() {
            take_pwstr(p)
        } else {
            String::new()
        }
    }

    let v4: ICoreWebView2_4 = web_view
        .cast()
        .map_err(|e| EngineError::Other(format!("cast to ICoreWebView2_4: {e}")))?;
    let downloads = downloads.clone();
    let handler = DownloadStartingEventHandler::create(Box::new(move |_sender, args| {
        let Some(args) = args else { return Ok(()) };
        let op: ICoreWebView2DownloadOperation = unsafe { args.DownloadOperation() }?;
        let url = read_pwstr(|p| unsafe { op.Uri(p) });
        if url.starts_with("blob:") || url.starts_with("data:") {
            unsafe { args.SetCancel(true) }?;
            return Ok(());
        }
        let cd = read_pwstr(|p| unsafe { op.ContentDisposition(p) });
        let mime = read_pwstr(|p| unsafe { op.MimeType(p) });
        let name = name_from_disposition(&cd).unwrap_or_default();
        let Ok(id) = downloads.lock().map(|mut d| d.begin(&url, &name, &mime)) else {
            return Ok(());
        };
        let mut total: i64 = 0;
        let _ = unsafe { op.TotalBytesToReceive(&raw mut total) };
        if u64::try_from(total).is_ok_and(|n| n > MAX_BYTES) {
            if let Ok(mut d) = downloads.lock() {
                d.finish(
                    id,
                    Err(format!("download exceeds {MAX_BYTES} bytes ({total})")),
                );
            }
            unsafe { args.SetCancel(true) }?;
            return Ok(());
        }
        let path = temp_path(id);
        let _ = std::fs::remove_file(&path);
        let wide = HSTRING::from(path.as_os_str());
        unsafe { args.SetResultFilePath(&wide) }?;
        unsafe { args.SetHandled(true) }?;

        let dl = downloads.clone();
        let state_handler = StateChangedEventHandler::create(Box::new(move |op, _| {
            let Some(op) = op else { return Ok(()) };
            let mut state = COREWEBVIEW2_DOWNLOAD_STATE::default();
            unsafe { op.State(&raw mut state) }?;
            let result = if state == COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED {
                let path = temp_path(id);
                let bytes =
                    std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()));
                let _ = std::fs::remove_file(&path);
                bytes
            } else if state == COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED {
                let _ = std::fs::remove_file(temp_path(id));
                let mut reason = COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON::default();
                let _ = unsafe { op.InterruptReason(&raw mut reason) };
                Err(format!("download interrupted ({reason:?})"))
            } else {
                return Ok(());
            };
            if let Ok(mut d) = dl.lock() {
                d.finish(id, result);
            }
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe { op.add_StateChanged(&state_handler, &raw mut token) }?;
        Ok(())
    }));
    let mut token: i64 = 0;
    unsafe { v4.add_DownloadStarting(&handler, &raw mut token) }
        .map_err(|e| EngineError::Other(format!("add_DownloadStarting: {e}")))?;
    Ok(())
}
