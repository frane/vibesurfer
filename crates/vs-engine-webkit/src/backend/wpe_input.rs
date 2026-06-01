//! Linux native input dispatch for the cursor primitives (`vs move-to`,
//! `click-at`, `hover-at`, `drag`).
//!
//! WebKitGTK 6 / GTK 4 deliberately removed public synthetic-event
//! construction (`GdkButtonEvent` and friends are ELF-hidden internal
//! symbols, not in `gdkevents.symbols.in`, so not even unsafe Rust can
//! reach them). The supported paths for injecting trusted input on
//! modern Linux are:
//!
//!  1. **XTest** — X11 protocol extension. `FakeInput` requests go
//!     through the X server and arrive at subscribed clients (our
//!     WebKitGTK WebView's hosting GtkWindow) as real hardware input.
//!     Trusted in JS (`isTrusted = true`). Works under `xvfb` (CI) and
//!     any X11 / Xwayland session.
//!  2. **libei** — Wayland emulated-input protocol. Negotiates a virtual
//!     pointer via the `xdg-desktop-portal` `RemoteDesktop` interface,
//!     then emits events that the compositor delivers as trusted
//!     hardware input. Required for pure-Wayland sessions where the
//!     compositor refuses to launch Xwayland.
//!
//! XTest goes through the pure-Rust `x11rb` crate: no `unsafe`, no
//! `dlopen`/`dlsym`, no libc. libei (Phase B) will route through the
//! similarly-safe `reis` crate. Runtime detection prefers libei under
//! pure Wayland and falls back to XTest otherwise; if neither path is
//! reachable, `cursor_op` returns `ENGINE_UNSUPPORTED` and the wire
//! response carries `! ENGINE_UNSUPPORTED` exactly the same way it did
//! before v0.1.11.
//!
//! Coordinate space: every dispatcher accepts screen-absolute CSS px.
//! The caller (the WebView's hosting `gtk::Window`) maintains the
//! `(window_origin_x, window_origin_y)` translation from the WebView's
//! local rect (top-left at 0,0) into screen coordinates.

use std::sync::OnceLock;

use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::Window;
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

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
/// over `x11rb`, ei_device events over libei, etc.). All methods are
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
/// or `None` if neither libei nor XTest could be reached.
///
/// Detection order:
///   1. If `XDG_SESSION_TYPE=wayland` AND libei portal-session opens:
///      return `Libei`. (Phase B — currently returns `None` from the
///      libei probe; see comment in `LibeiDispatcher::try_new`.)
///   2. Else if `RustConnection::connect` succeeds against the
///      `DISPLAY` env: return `Xtest`.
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

/// Cached dispatcher so repeated `cursor_op` calls reuse the X
/// connection / libei session instead of reconnecting each call.
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
    DISPATCHER
        .get_or_init(detect)
        .as_deref()
        .map(InputDispatcher::backend_name)
}

// =============================================================================
// XTest backend (x11rb pure-Rust X11 client)
// =============================================================================

/// XTest event-type codes used in the `FakeInput` request. Mirror of
/// X11 protocol constants — `x11rb` doesn't expose them as named
/// constants under `xtest`, only under `xproto::ButtonPressEvent` /
/// friends, which would require pulling more types just to get u8s.
const XT_MOTION_NOTIFY: u8 = 6;
const XT_BUTTON_PRESS: u8 = 4;
const XT_BUTTON_RELEASE: u8 = 5;

/// XTest dispatcher. Holds an `x11rb::rust_connection::RustConnection`
/// to the default display plus the root window id (used as the target
/// in `FakeInput` motion requests).
struct XtestDispatcher {
    conn: RustConnection,
    root: Window,
}

impl XtestDispatcher {
    fn try_new() -> Option<Self> {
        // `connect(None)` reads `$DISPLAY`. The first roots[0] is the
        // default screen — same convention every X11 client uses.
        let (conn, screen_num) = RustConnection::connect(None).ok()?;
        let root = conn.setup().roots.get(screen_num)?.root;
        Some(Self { conn, root })
    }
}

impl InputDispatcher for XtestDispatcher {
    fn backend_name(&self) -> &'static str {
        "xtest"
    }
    fn dispatch(&self, ev: InputEvent) -> EngineResult<()> {
        let (type_, detail, root_x, root_y) = match ev {
            InputEvent::Move(p) => (
                XT_MOTION_NOTIFY,
                0_u8,
                clamp_i16(p.x),
                clamp_i16(p.y),
            ),
            InputEvent::Press(b) => (XT_BUTTON_PRESS, button_code(b), 0, 0),
            InputEvent::Release(b) => (XT_BUTTON_RELEASE, button_code(b), 0, 0),
        };
        // FakeInput is fire-and-forget: ignore the void cookie's
        // error reply; any server-side error surfaces on the next
        // request anyway.
        self.conn
            .xtest_fake_input(type_, detail, CURRENT_TIME, self.root, root_x, root_y, 0)
            .map_err(|e| EngineError::Other(format!("xtest_fake_input: {e}")))?
            .ignore_error();
        Ok(())
    }
    fn flush(&self) -> EngineResult<()> {
        self.conn
            .flush()
            .map_err(|e| EngineError::Other(format!("XFlush: {e}")))?;
        // `sync` forces a round-trip so any deferred server error
        // surfaces here rather than on the next `cursor_op`.
        self.conn
            .sync()
            .map_err(|e| EngineError::Other(format!("XSync: {e}")))?;
        Ok(())
    }
}

#[allow(clippy::cast_possible_truncation)]
fn clamp_i16(v: i32) -> i16 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn button_code(b: Button) -> u8 {
    // X11 button numbering: 1=left, 2=middle, 3=right.
    match b {
        Button::Left => 1,
        Button::Middle => 2,
        Button::Right => 3,
    }
}

// =============================================================================
// libei backend (Phase B scaffold)
// =============================================================================

/// libei dispatcher. **Not yet implemented:** libei requires a
/// `xdg-desktop-portal` `RemoteDesktop` session (D-Bus) to obtain the
/// libei socket FD from the compositor, plus an event loop for
/// seat / device-resumed / sequence handshakes before the first
/// pointer event flows. That plumbing lands in v0.1.12 via the
/// `reis` crate; the v0.1.11 detection already prefers libei under
/// pure Wayland so the upgrade flips on automatically once `try_new`
/// starts returning `Some`.
///
/// Today `try_new` always returns `None`, so pure-Wayland users
/// without Xwayland get `ENGINE_UNSUPPORTED`; users on X11 / Xwayland
/// get XTest.
struct LibeiDispatcher;

impl LibeiDispatcher {
    fn try_new() -> Option<Self> {
        // Phase B: open xdg-desktop-portal RemoteDesktop session via
        // zbus, hand the returned FD to `reis::ei::Context`, register
        // a pointer device, dispatch through it. A single-file change
        // so detection scaffolding can stay here.
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
