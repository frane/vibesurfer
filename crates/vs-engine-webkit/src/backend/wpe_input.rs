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
// libei backend (xdg-desktop-portal RemoteDesktop + ashpd)
// =============================================================================
//
// We don't talk to libei directly — `ashpd`'s `RemoteDesktop` portal
// exposes `notify_pointer_motion_absolute` and `notify_pointer_button`
// as D-Bus methods, which the compositor delivers to focused windows
// as trusted hardware input. This is the recommended path for
// non-interactive applications on modern Wayland sessions (GNOME 41+,
// KDE Plasma 5.27+, sway via wlroots-virtual-pointer).
//
// We park a current-thread tokio runtime on a dedicated thread, run
// the portal session there, and `block_on` for every `dispatch` call.
// The portal session lifecycle (create_session → select_devices →
// start) happens once at process startup; the user sees a one-time
// permission prompt from their compositor. Successive `dispatch`
// calls are cheap D-Bus method calls.

use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop};
use ashpd::desktop::{PersistMode, Session};
use enumflags2::BitFlags;
use std::sync::mpsc;

/// libei (xdg-desktop-portal RemoteDesktop) dispatcher.
struct LibeiDispatcher {
    /// Channel into the dedicated portal-session thread. Owned
    /// `tokio` runtime stays on that thread; we don't move it.
    cmd_tx: mpsc::Sender<LibeiCmd>,
}

enum LibeiCmd {
    Motion { x: f64, y: f64, ack: mpsc::Sender<EngineResult<()>> },
    Button { code: u32, pressed: bool, ack: mpsc::Sender<EngineResult<()>> },
}

impl LibeiDispatcher {
    fn try_new() -> Option<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<LibeiCmd>();
        let (init_tx, init_rx) = mpsc::sync_channel::<Option<()>>(1);
        std::thread::Builder::new()
            .name("vs-libei".to_string())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    let _ = init_tx.send(None);
                    return;
                };
                rt.block_on(async move {
                    let proxy = match RemoteDesktop::new().await {
                        Ok(p) => p,
                        Err(_) => {
                            let _ = init_tx.send(None);
                            return;
                        }
                    };
                    let session: Session<'_, RemoteDesktop> = match proxy.create_session().await {
                        Ok(s) => s,
                        Err(_) => {
                            let _ = init_tx.send(None);
                            return;
                        }
                    };
                    let types: BitFlags<DeviceType> = DeviceType::Pointer.into();
                    if proxy
                        .select_devices(&session, types, None, PersistMode::DoNot)
                        .await
                        .and_then(|r| r.response())
                        .is_err()
                    {
                        let _ = init_tx.send(None);
                        return;
                    }
                    // `start` triggers the compositor's consent prompt
                    // (one-time, per session) and blocks until the
                    // user approves or denies.
                    if proxy
                        .start(&session, None)
                        .await
                        .and_then(|r| r.response())
                        .is_err()
                    {
                        let _ = init_tx.send(None);
                        return;
                    }
                    let _ = init_tx.send(Some(()));
                    while let Ok(cmd) = cmd_rx.recv() {
                        match cmd {
                            LibeiCmd::Motion { x, y, ack } => {
                                let r = proxy
                                    .notify_pointer_motion_absolute(&session, 0, x, y)
                                    .await
                                    .map_err(|e| EngineError::Other(format!("ei motion: {e}")));
                                let _ = ack.send(r);
                            }
                            LibeiCmd::Button { code, pressed, ack } => {
                                let state = if pressed {
                                    ashpd::desktop::remote_desktop::KeyState::Pressed
                                } else {
                                    ashpd::desktop::remote_desktop::KeyState::Released
                                };
                                #[allow(clippy::cast_possible_wrap)]
                                let r = proxy
                                    .notify_pointer_button(&session, code as i32, state)
                                    .await
                                    .map_err(|e| EngineError::Other(format!("ei button: {e}")));
                                let _ = ack.send(r);
                            }
                        }
                    }
                });
            })
            .ok()?;
        // Block on the portal handshake completing. If the compositor
        // doesn't have a RemoteDesktop portal, or the user denies,
        // the worker thread returns None and we fall through to XTest.
        init_rx.recv().ok().flatten()?;
        Some(Self { cmd_tx })
    }
}

impl InputDispatcher for LibeiDispatcher {
    fn backend_name(&self) -> &'static str {
        "libei"
    }
    fn dispatch(&self, ev: InputEvent) -> EngineResult<()> {
        let (ack_tx, ack_rx) = mpsc::channel();
        let cmd = match ev {
            InputEvent::Move(p) => LibeiCmd::Motion {
                x: f64::from(p.x),
                y: f64::from(p.y),
                ack: ack_tx,
            },
            InputEvent::Press(b) => LibeiCmd::Button {
                code: linux_button_code(b),
                pressed: true,
                ack: ack_tx,
            },
            InputEvent::Release(b) => LibeiCmd::Button {
                code: linux_button_code(b),
                pressed: false,
                ack: ack_tx,
            },
        };
        self.cmd_tx
            .send(cmd)
            .map_err(|_| EngineError::Other("libei worker thread gone".into()))?;
        ack_rx
            .recv()
            .map_err(|_| EngineError::Other("libei ack channel closed".into()))?
    }
    fn flush(&self) -> EngineResult<()> {
        Ok(())
    }
}

/// Linux input subsystem button codes (`linux/input-event-codes.h`).
/// `BTN_LEFT` = 0x110, `BTN_MIDDLE` = 0x112, `BTN_RIGHT` = 0x111. The
/// RemoteDesktop portal expects these, not the X11 1/2/3 numbering.
fn linux_button_code(b: Button) -> u32 {
    match b {
        Button::Left => 0x110,
        Button::Middle => 0x112,
        Button::Right => 0x111,
    }
}
