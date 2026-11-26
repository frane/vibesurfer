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

mod libei;
mod xtest;

use std::sync::OnceLock;
use std::time::Duration;

use crate::engine::{EngineError, EngineResult};

use libei::LibeiDispatcher;
use xtest::XtestDispatcher;

/// A pointer position in screen-absolute CSS pixels.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScreenPoint {
    pub x: i32,
    pub y: i32,
}

/// Mouse button identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Button {
    Left,
    Middle,
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
    /// Turn the wheel by `(dx, dy)` CSS px at the pointer (positive
    /// `dy` scrolls down). `step` is what one wheel click scrolls on
    /// each axis, for dispatchers that only have clicks.
    Wheel { dx: f64, dy: f64, step: (f64, f64) },
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

    /// Press one character as a real key.
    ///
    /// `settle` pumps the caller's main loop for the given duration.
    /// The dispatcher needs it because making a character typeable can
    /// mean changing the server's keymap, and the toolkit only learns
    /// about that by processing an event — on the same thread this
    /// call is running on. Sleeping here instead would mean the key
    /// arrives before anything can interpret it.
    ///
    /// A dispatcher that cannot reach a keyboard says so, and the wire
    /// reports `ENGINE_UNSUPPORTED` exactly as the whole primitive did
    /// before.
    fn key_down(&self, _ch: char, _settle: &dyn Fn(Duration)) -> EngineResult<()> {
        Err(EngineError::Unsupported {
            engine: "wpe",
            primitive: "type_text",
        })
    }

    /// Release the character most recently pressed by [`Self::key_down`].
    fn key_up(&self, _ch: char) -> EngineResult<()> {
        Err(EngineError::Unsupported {
            engine: "wpe",
            primitive: "type_text",
        })
    }

    /// Release any resources a run of [`Self::type_char`] set up. The
    /// XTest path borrows a keycode from the server's keymap, so this
    /// is where it gives it back; other paths do nothing.
    fn end_typing(&self) -> EngineResult<()> {
        Ok(())
    }

    /// Point the keyboard at whatever the pointer is over, if the
    /// platform routes keys by focus rather than by position. Called
    /// before a run of typing; best-effort, and a failure here is not
    /// worth failing the call over.
    fn focus_pointer_window(&self) {}

    /// Hold (`pressed`) or let go of a modifier key, by X keysym
    /// (Shift_L, Control_L, Alt_L, Super_L), for a click with
    /// modifiers.
    fn modifier(&self, _keysym: u32, _pressed: bool) -> EngineResult<()> {
        Err(EngineError::Unsupported {
            engine: "wpe",
            primitive: "click modifiers",
        })
    }

    /// Put the top-level window titled `title` above the others, where
    /// windows share one screen position and input goes by position.
    /// Best-effort, like focus.
    fn raise_window(&self, _title: &str) {}

    /// The screen's size in pixels, where the platform has one screen
    /// that input addresses by position.
    fn screen_size(&self) -> Option<(i32, i32)> {
        None
    }

    /// Move the top-level window titled `title` to (`x`, `y`) on the
    /// screen. Returns whether it was found.
    fn place_window(&self, _title: &str, _x: i32, _y: i32) -> bool {
        false
    }

    /// Where the top-level window titled `title` sits on the screen now
    /// (its top-left corner), as the display server reports it.
    fn window_origin(&self, _title: &str) -> Option<(i32, i32)> {
        None
    }

    /// The X window id of the top-level window titled `title`, where
    /// the display is X11 (the target of an XDND drop).
    fn x11_window(&self, _title: &str) -> Option<u32> {
        None
    }

    /// Route the input that follows through the pointer and keyboard
    /// that belong to the page named `title` (created on first use), so
    /// pages do not share one pointer position, hover or focus. Where
    /// the platform has one pointer for everything, this does nothing.
    fn use_page_pointer(&self, _title: &str) {}

    /// Remove the pointer and keyboard of a page that closed.
    fn drop_page_pointer(&self, _title: &str) {}

    /// Route the input that follows through the display's core pointer
    /// and keyboard again. Typing uses these: toolkits take keys from
    /// the core keyboard only.
    fn use_core_pointer(&self) {}
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
/// in `wpe`, which propagates the error as `ENGINE_UNSUPPORTED` on
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

/// The X keysym that produces `ch`.
///
/// Latin-1 is the one range where keysyms and Unicode agree by
/// historical accident, so it passes through. Everything else uses the
/// Unicode escape range the X protocol reserves for exactly this. The
/// few control characters worth typing have named keysyms and no
/// printable form, so they are spelled out.
pub(super) fn keysym_for(ch: char) -> u32 {
    match ch {
        '\u{8}' => 0xff08,     // BackSpace
        '\t' => 0xff09,        // Tab
        '\n' | '\r' => 0xff0d, // Return
        '\u{1b}' => 0xff1b,    // Escape
        _ => {
            let cp = ch as u32;
            if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) {
                cp
            } else {
                0x0100_0000 + cp
            }
        }
    }
}
