//! Linux native input dispatch for the cursor primitives (`vs move-to`,
//! `click-at`, `hover-at`, `drag`).
//!
//! WebKitGTK 6 / GTK 4 deliberately removed public synthetic-event
//! construction (`GdkButtonEvent` and friends are ELF-hidden internal
//! symbols, not in `gdkevents.symbols.in`, so even `unsafe` Rust can't
//! `dlsym` them). The supported paths for injecting trusted input on
//! modern Linux are:
//!
//!  1. **XTest** — X11 protocol extension. `XTestFakeMotionEvent` /
//!     `XTestFakeButtonEvent` go through the X server and arrive at
//!     subscribed clients (our WebKitGTK WebView's hosting GtkWindow)
//!     as real hardware input. Trusted in JS (`isTrusted = true`).
//!     Works under `xvfb` (CI) and any X11 / Xwayland session.
//!  2. **libei** — Wayland emulated-input protocol. Negotiates a virtual
//!     pointer via the `xdg-desktop-portal` `RemoteDesktop` interface,
//!     then emits events that the compositor delivers as trusted
//!     hardware input. Required for pure-Wayland sessions where the
//!     compositor refuses to launch Xwayland.
//!
//! Both libraries are loaded via raw `libc::dlopen`/`dlsym` so neither
//! becomes a build-time Cargo dependency — the binary still links on a
//! host that has only one (or neither) installed. Runtime detection
//! prefers libei under pure Wayland and falls back to XTest otherwise;
//! if neither loads, `cursor_op` returns `ENGINE_UNSUPPORTED` and the
//! agent's wire response carries `! ENGINE_UNSUPPORTED` exactly the
//! same way it did before v0.1.11.
//!
//! Coordinate space: every dispatcher accepts screen-absolute CSS px.
//! The caller (the WebView's hosting GtkWindow) maintains the
//! `(window_origin_x, window_origin_y)` translation from the WebView's
//! local rect (top-left at 0,0) into screen coordinates.

use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CString};
use std::sync::OnceLock;
use std::time::Duration;

use crate::engine::{EngineError, EngineResult};

/// A pointer position in screen-absolute CSS pixels.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScreenPoint {
    pub x: i32,
    pub y: i32,
}

/// Mouse button identifier. Only `Left` is wired by the cursor
/// primitives today; `Middle` / `Right` exist so a future
/// right-click primitive can drop into the same dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Button {
    Left,
    #[allow(dead_code)]
    Middle,
    #[allow(dead_code)]
    Right,
}

/// One injected event in a humanized sequence.
#[derive(Debug, Clone, Copy)]
pub(crate) enum InputEvent {
    /// Move the pointer to the given screen position.
    Move(ScreenPoint),
    /// Press a mouse button at the pointer's current position.
    Press(Button),
    /// Release a mouse button at the pointer's current position.
    Release(Button),
}

/// A pluggable native-input dispatcher.
///
/// Implementors translate `InputEvent`s into platform calls (XTest
/// over libXtst, ei_device events over libei, etc.). All methods are
/// fallible because the underlying platform call may fail at runtime
/// (display closed, compositor revoked the session, kernel busy).
pub(crate) trait InputDispatcher: Send + Sync {
    /// Best-effort name for diagnostics / `EngineCapabilities`.
    fn backend_name(&self) -> &'static str;
    /// Dispatch a single event. Implementations may buffer; call
    /// `flush` to guarantee delivery.
    fn dispatch(&self, ev: InputEvent) -> EngineResult<()>;
    /// Force any buffered events to flush to the server / compositor.
    fn flush(&self) -> EngineResult<()>;
}

// =============================================================================
// Runtime detection
// =============================================================================

/// Return the best available input dispatcher for the current session,
/// or `None` if neither libei nor libXtst could be loaded.
///
/// Detection order:
///   1. If `XDG_SESSION_TYPE=wayland` AND libei loads AND a portal
///      session can be opened: return `Libei`. (Phase B — currently
///      returns `None` from the libei probe; see comment in
///      `LibeiDispatcher::try_new`.)
///   2. Else if libXtst loads against an open X display: return `Xtest`.
///   3. Else: `None`.
pub(crate) fn detect() -> Option<Box<dyn InputDispatcher>> {
    if is_wayland_session() {
        if let Some(b) = LibeiDispatcher::try_new() {
            return Some(Box::new(b));
        }
    }
    if let Some(b) = XtestDispatcher::try_new() {
        return Some(Box::new(b));
    }
    None
}

fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        && std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
}

/// Cached dispatcher so repeated `cursor_op` calls reuse the X display
/// / libei session instead of re-loading the library each call.
static DISPATCHER: OnceLock<Option<Box<dyn InputDispatcher + 'static>>> = OnceLock::new();

/// Resolve (and cache) the dispatcher. Returns `Err(Unsupported)` if
/// neither backend is available — the caller is the `cursor_op` impl
/// in `wpe.rs`, which propagates the error as `ENGINE_UNSUPPORTED` on
/// the wire.
pub(crate) fn dispatcher() -> EngineResult<&'static (dyn InputDispatcher + 'static)> {
    let cell = DISPATCHER.get_or_init(detect);
    match cell.as_deref() {
        Some(d) => Ok(d),
        None => Err(EngineError::Unsupported {
            engine: "wpe",
            primitive: "cursor_op",
        }),
    }
}

/// Public-side helper used by `wpe.rs::WpeBackend::capabilities()`:
/// returns the backend name (`"xtest"` / `"libei"`) if input dispatch
/// is available, or `None` if it isn't.
#[allow(dead_code)]
pub(crate) fn active_backend_name() -> Option<&'static str> {
    DISPATCHER.get_or_init(detect).as_deref().map(|d| d.backend_name())
}

// =============================================================================
// XTest backend (libXtst.so.6 via dlopen)
// =============================================================================

type XOpenDisplayFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type XCloseDisplayFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type XFlushFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type XSyncFn = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type XTestFakeMotionEventFn =
    unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int, c_ulong) -> c_int;
type XTestFakeButtonEventFn = unsafe extern "C" fn(*mut c_void, c_uint, c_int, c_ulong) -> c_int;

/// XTest dispatcher. Holds dlopen handles for `libX11.so.6` and
/// `libXtst.so.6` plus the open `Display*`. `Send` because the handles
/// are opaque pointers without thread-local state — but in practice
/// only the daemon's engine thread calls into it, matching the rest of
/// `WpeBackend`.
struct XtestDispatcher {
    // `_libx11` / `_libxtst` keep the dlopen handles alive for the
    // lifetime of `DISPATCHER`. Closing them would invalidate the
    // function pointers below.
    _libx11: *mut c_void,
    _libxtst: *mut c_void,
    display: *mut c_void,
    x_flush: XFlushFn,
    x_sync: XSyncFn,
    fake_motion: XTestFakeMotionEventFn,
    fake_button: XTestFakeButtonEventFn,
}

// SAFETY: the wrapped pointers are opaque handles; XTest's symbols
// re-enter the X server which serializes per-display. We only ever
// touch the dispatcher from the engine thread, so concurrent access
// isn't a concern in practice.
unsafe impl Send for XtestDispatcher {}
unsafe impl Sync for XtestDispatcher {}

impl XtestDispatcher {
    fn try_new() -> Option<Self> {
        unsafe {
            let libx11 = libc::dlopen(c"libX11.so.6".as_ptr(), libc::RTLD_LAZY);
            if libx11.is_null() {
                return None;
            }
            let libxtst = libc::dlopen(c"libXtst.so.6".as_ptr(), libc::RTLD_LAZY);
            if libxtst.is_null() {
                libc::dlclose(libx11);
                return None;
            }
            let open_display: XOpenDisplayFn = std::mem::transmute(load_sym(libx11, c"XOpenDisplay")?);
            let close_display: XCloseDisplayFn =
                std::mem::transmute(load_sym(libx11, c"XCloseDisplay")?);
            let x_flush: XFlushFn = std::mem::transmute(load_sym(libx11, c"XFlush")?);
            let x_sync: XSyncFn = std::mem::transmute(load_sym(libx11, c"XSync")?);
            let fake_motion: XTestFakeMotionEventFn =
                std::mem::transmute(load_sym(libxtst, c"XTestFakeMotionEvent")?);
            let fake_button: XTestFakeButtonEventFn =
                std::mem::transmute(load_sym(libxtst, c"XTestFakeButtonEvent")?);

            // Default display (DISPLAY env var) — same one the
            // hosting GtkWindow opens.
            let display = open_display(std::ptr::null());
            if display.is_null() {
                libc::dlclose(libxtst);
                libc::dlclose(libx11);
                return None;
            }
            // We don't call close_display in normal operation (we hold
            // the dispatcher for process lifetime). Keep the symbol
            // resolved anyway so a future explicit shutdown path can
            // use it — silences "unused".
            let _ = close_display;

            Some(Self {
                _libx11: libx11,
                _libxtst: libxtst,
                display,
                x_flush,
                x_sync,
                fake_motion,
                fake_button,
            })
        }
    }
}

impl InputDispatcher for XtestDispatcher {
    fn backend_name(&self) -> &'static str {
        "xtest"
    }
    fn dispatch(&self, ev: InputEvent) -> EngineResult<()> {
        unsafe {
            match ev {
                InputEvent::Move(p) => {
                    // screen_number = -1 means "use the screen the
                    // pointer is currently on"; delay = 0.
                    (self.fake_motion)(self.display, -1, p.x, p.y, 0);
                }
                InputEvent::Press(b) => {
                    (self.fake_button)(self.display, button_code(b), 1, 0);
                }
                InputEvent::Release(b) => {
                    (self.fake_button)(self.display, button_code(b), 0, 0);
                }
            }
            (self.x_flush)(self.display);
        }
        Ok(())
    }
    fn flush(&self) -> EngineResult<()> {
        unsafe {
            // XSync(false) blocks until the X server has processed
            // every pending request — useful before reading state.
            (self.x_sync)(self.display, 0);
        }
        Ok(())
    }
}

fn button_code(b: Button) -> c_uint {
    // X11 mouse button numbering: 1=left, 2=middle, 3=right.
    match b {
        Button::Left => 1,
        Button::Middle => 2,
        Button::Right => 3,
    }
}

// =============================================================================
// libei backend (libei.so.1 via dlopen) — Phase B scaffold
// =============================================================================

/// libei dispatcher. **Not yet implemented:** the libei protocol
/// requires a `xdg-desktop-portal` `RemoteDesktop` session (D-Bus) to
/// obtain the libei socket FD from the compositor, plus an event loop
/// to handle seat / device-resumed / sequence callbacks before the
/// first emit. That plumbing lands in v0.1.12; the v0.1.11 detection
/// already prefers libei under pure Wayland so the upgrade flips on
/// automatically once `try_new` starts returning `Some`.
///
/// Today `try_new` always returns `None`, so detection falls through
/// to XTest (which on Wayland sessions means Xwayland — works wherever
/// the user hasn't explicitly disabled Xwayland).
struct LibeiDispatcher {
    #[allow(dead_code)]
    _libei: *mut c_void,
}

// SAFETY: see `XtestDispatcher` — same single-engine-thread access
// pattern. The unsafe impls are inert until `try_new` returns Some.
unsafe impl Send for LibeiDispatcher {}
unsafe impl Sync for LibeiDispatcher {}

impl LibeiDispatcher {
    fn try_new() -> Option<Self> {
        // Phase B: implement the portal-session + libei wire here.
        // Returning None today means pure-Wayland users without
        // Xwayland get `ENGINE_UNSUPPORTED`; users on X11 / Xwayland
        // get XTest. Detection scaffolding stays here so phase B is
        // a single-file change.
        None
    }
}

impl InputDispatcher for LibeiDispatcher {
    fn backend_name(&self) -> &'static str {
        "libei"
    }
    fn dispatch(&self, _ev: InputEvent) -> EngineResult<()> {
        Err(EngineError::Unsupported {
            engine: "wpe",
            primitive: "cursor_op:libei",
        })
    }
    fn flush(&self) -> EngineResult<()> {
        Ok(())
    }
}

// =============================================================================
// Shared dlopen helper
// =============================================================================

/// Resolve a symbol from a dlopen handle. Returns `None` if the symbol
/// isn't exported — used during library probing so a missing function
/// causes the whole backend to be skipped instead of crashing later.
unsafe fn load_sym(handle: *mut c_void, name: &core::ffi::CStr) -> Option<*mut c_void> {
    let p = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if p.is_null() {
        None
    } else {
        Some(p)
    }
}

// =============================================================================
// Humanized path -> InputEvent sequence
// =============================================================================

/// Translate a Bezier mouse path from `vs-humanize` into an
/// `InputEvent` sequence. The caller pumps the sequence into the
/// dispatcher one event at a time, sleeping between events to honor
/// the path's per-step timing — same shape as
/// `webkit::input::move_along_path` on macOS.
#[allow(dead_code)]
pub(crate) fn path_to_events(
    path: &[vs_humanize::MouseStep],
    window_origin: (i32, i32),
) -> Vec<(InputEvent, Duration)> {
    let mut out = Vec::with_capacity(path.len());
    let mut prev_ms: u128 = 0;
    for step in path {
        let dt = step.at.as_millis().saturating_sub(prev_ms);
        let pt = ScreenPoint {
            x: window_origin.0 + step.point.x.round() as i32,
            y: window_origin.1 + step.point.y.round() as i32,
        };
        let ev = match step.kind {
            vs_humanize::MouseStepKind::Move => InputEvent::Move(pt),
            vs_humanize::MouseStepKind::Down => InputEvent::Press(Button::Left),
            vs_humanize::MouseStepKind::Up => InputEvent::Release(Button::Left),
            vs_humanize::MouseStepKind::Click => {
                // `Click` from the humanize sequence is emitted as a
                // tightly-paired Down+Up by the caller; we surface
                // only the Down here and rely on the caller to append
                // the Up. In practice `vs-humanize` only emits `Click`
                // alongside an explicit Down/Up pair, so this branch
                // is unreachable today — return Move as a safe no-op.
                InputEvent::Move(pt)
            }
        };
        out.push((ev, Duration::from_millis(u64::try_from(dt).unwrap_or(0))));
        prev_ms = step.at.as_millis();
    }
    out
}

// Silence "unused" on the CString import on toolchains that fold it
// away; the type is referenced indirectly via `c"..."` literals which
// expand to `&CStr`, not `CString`.
#[allow(dead_code)]
const _: fn() = || {
    let _: CString;
};
