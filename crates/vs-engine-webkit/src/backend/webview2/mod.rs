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
//! Implements every primitive on [`Engine`]. The `engine-tests` workflow
//! runs the real-browser cell suite on it on `windows-latest`. Built only
//! for Windows.
//!
//! Capability flags follow the same per-page-bool + aggregate pattern
//! Mac (`webkit/mod.rs`) and Linux (`wpe/mod.rs`) use: `inspector_*`
//! goes `true` only when the install path actually succeeded.

mod capture;
mod cookies;
mod engine_impl;
mod host;
mod input;
mod script;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use super::native_download::SharedDownloads;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2, ICoreWebView2CompositionController, ICoreWebView2Controller,
    ICoreWebView2Environment,
};
use windows::core::HSTRING;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::DirectComposition::{
    IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, RegisterClassW, HWND_MESSAGE, WINDOW_EX_STYLE, WNDCLASSW,
    WS_OVERLAPPED,
};

use crate::backend::inspector_bridge::InspectorSlots;
use crate::engine::{Engine, EngineError, EngineResult, PageHandle};

use script::execute_script;

// =============================================================================
// Per-page state
// =============================================================================

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
    /// DirectComposition device — owns the visual tree the WebView2
    /// renders into. `Commit` was called once at setup; we hold the
    /// device so the visual tree stays alive for the page's lifetime.
    _dcomp_device: IDCompositionDevice,
    /// Composition target bound to `host`.
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
    /// The page's message-only host window. Last, so it is destroyed
    /// after the composition objects bound to it are released.
    _host: HostWindow,
}

/// A page's message-only host window, destroyed when dropped. Every
/// page owns one (see [`new_host_hwnd`]), so closing a page, or failing
/// to build one, does not leave a window behind.
pub(super) struct HostWindow(HWND);

impl HostWindow {
    fn hwnd(&self) -> HWND {
        self.0
    }
}

impl Drop for HostWindow {
    fn drop(&mut self) {
        // SAFETY: the window was created on this thread by
        // `new_host_hwnd` and is destroyed exactly once, here.
        let _ = unsafe { DestroyWindow(self.0) };
    }
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
}

/// Create a fresh message-only HWND to host a page's WebView2
/// composition target. DirectComposition rejects a second
/// `CreateTargetForHwnd` on a window that already has a target
/// (`DCOMPOSITION_ERROR_WINDOW_ALREADY_COMPOSED`), so every page gets its
/// own HWND under the same shared class.
fn new_host_hwnd() -> EngineResult<HostWindow> {
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
        Ok(HostWindow(hwnd))
    }
}
