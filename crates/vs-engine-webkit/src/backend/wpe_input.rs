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

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{ConnectionExt as _, InputFocus, Window};
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
const XT_KEY_PRESS: u8 = 2;
const XT_KEY_RELEASE: u8 = 3;

/// XTest dispatcher. Holds an `x11rb::rust_connection::RustConnection`
/// to the default display plus the root window id (used as the target
/// in `FakeInput` motion requests).
struct XtestDispatcher {
    conn: RustConnection,
    root: Window,
    /// The keycode borrowed from the server's keymap for typing, and
    /// the keysym currently bound to it. See [`XtestDispatcher::bind`].
    scratch: Mutex<Option<ScratchKey>>,
}

/// A keycode borrowed from the X server's keymap so arbitrary text
/// can be typed through it.
#[derive(Clone, Copy)]
struct ScratchKey {
    keycode: u8,
    /// What it is bound to right now, so typing "aaa" rebinds once
    /// rather than three times.
    bound: u32,
    /// How many keysyms per keycode this server's keymap uses, needed
    /// to hand the keycode back in the shape it was found.
    per_code: u8,
}

impl XtestDispatcher {
    fn try_new() -> Option<Self> {
        // `connect(None)` reads `$DISPLAY`. The first roots[0] is the
        // default screen — same convention every X11 client uses.
        let (conn, screen_num) = RustConnection::connect(None).ok()?;
        let root = conn.setup().roots.get(screen_num)?.root;
        Some(Self {
            conn,
            root,
            scratch: Mutex::new(None),
        })
    }

    /// Find a keycode the current keymap leaves empty, so binding it
    /// takes nothing away from the user.
    ///
    /// Typing arbitrary text through XTest means naming a keycode,
    /// and a keycode only means a character because the keymap says
    /// so. Rather than hunt for the keycode that happens to produce
    /// `ß` on this layout — and then work out which modifiers it
    /// needs — we borrow an unused one and bind it to whatever we are
    /// about to type. This is what `xdotool` does for the same
    /// reason. [`Self::end_typing`] gives it back.
    fn find_scratch(&self) -> EngineResult<ScratchKey> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let count = max - min + 1;
        let map = self
            .conn
            .get_keyboard_mapping(min, count)
            .map_err(|e| EngineError::Other(format!("GetKeyboardMapping: {e}")))?
            .reply()
            .map_err(|e| EngineError::Other(format!("GetKeyboardMapping reply: {e}")))?;
        let per = map.keysyms_per_keycode as usize;
        if per == 0 {
            return Err(EngineError::Other("empty X keymap".into()));
        }
        // Scan from the top: high keycodes are where layouts leave
        // gaps, and a low free keycode is likelier to be one the
        // session is about to start using.
        for i in (0..usize::from(count)).rev() {
            let syms = &map.keysyms[i * per..(i + 1) * per];
            if syms.iter().all(|&k| k == 0) {
                return Ok(ScratchKey {
                    keycode: min + u8::try_from(i).unwrap_or(0),
                    bound: 0,
                    per_code: map.keysyms_per_keycode,
                });
            }
        }
        Err(EngineError::Other(
            "no free X keycode to type through".into(),
        ))
    }

    /// Bind the scratch keycode to `keysym`, if it is not already.
    fn bind(&self, keysym: u32, settle: &dyn Fn(Duration)) -> EngineResult<u8> {
        let mut guard = self.scratch.lock().unwrap();
        let mut key = match *guard {
            Some(k) => k,
            None => self.find_scratch()?,
        };
        if key.bound != keysym {
            // Every level gets the same keysym, so the character
            // arrives whatever the modifier state happens to be.
            let syms = vec![keysym; usize::from(key.per_code)];
            self.conn
                .change_keyboard_mapping(1, key.keycode, key.per_code, &syms)
                .map_err(|e| EngineError::Other(format!("ChangeKeyboardMapping: {e}")))?
                .check()
                .map_err(|e| EngineError::Other(format!("ChangeKeyboardMapping check: {e}")))?;
            // Clients learn the new mapping from a MappingNotify, and
            // the toolkit only sees that by running its event loop —
            // which is this thread. Pumping rather than sleeping is
            // the difference between the key arriving as the character
            // we just bound and arriving as whatever was there before.
            settle(Duration::from_millis(12));
            key.bound = keysym;
            *guard = Some(key);
        }
        Ok(key.keycode)
    }
}

impl InputDispatcher for XtestDispatcher {
    fn backend_name(&self) -> &'static str {
        "xtest"
    }
    fn dispatch(&self, ev: InputEvent) -> EngineResult<()> {
        let (type_, detail, root_x, root_y) = match ev {
            InputEvent::Move(p) => (XT_MOTION_NOTIFY, 0_u8, clamp_i16(p.x), clamp_i16(p.y)),
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

    fn key_down(&self, ch: char, settle: &dyn Fn(Duration)) -> EngineResult<()> {
        let keycode = self.bind(keysym_for(ch), settle)?;
        self.conn
            .xtest_fake_input(XT_KEY_PRESS, keycode, CURRENT_TIME, self.root, 0, 0, 0)
            .map_err(|e| EngineError::Other(format!("xtest key press: {e}")))?
            .ignore_error();
        self.flush()
    }

    fn key_up(&self, _ch: char) -> EngineResult<()> {
        // The binding from `key_down` is still in place, so the
        // release names the same keycode.
        let keycode = match *self.scratch.lock().unwrap() {
            Some(k) => k.keycode,
            None => return Ok(()),
        };
        self.conn
            .xtest_fake_input(XT_KEY_RELEASE, keycode, CURRENT_TIME, self.root, 0, 0, 0)
            .map_err(|e| EngineError::Other(format!("xtest key release: {e}")))?
            .ignore_error();
        self.flush()
    }

    fn end_typing(&self) -> EngineResult<()> {
        let mut guard = self.scratch.lock().unwrap();
        let Some(key) = guard.take() else {
            return Ok(());
        };
        // Hand the keycode back empty, the way it was found. Leaving
        // a stray binding behind would change what the user's own
        // keyboard does with that keycode for the rest of the session.
        let syms = vec![0_u32; usize::from(key.per_code)];
        self.conn
            .change_keyboard_mapping(1, key.keycode, key.per_code, &syms)
            .map_err(|e| EngineError::Other(format!("ChangeKeyboardMapping restore: {e}")))?
            .check()
            .map_err(|e| EngineError::Other(format!("ChangeKeyboardMapping restore check: {e}")))?;
        Ok(())
    }

    fn focus_pointer_window(&self) {
        // X routes keys by focus, not by pointer position, and a
        // headless session under xvfb has no window manager to set
        // focus for us. The caller has just clicked to place the
        // caret, so the window under the pointer is the one that
        // should hear the typing. Entirely best-effort: on a session
        // that does have a WM, focus is already where it belongs.
        let Ok(reply) = self.conn.query_pointer(self.root) else {
            return;
        };
        let Ok(reply) = reply.reply() else { return };
        if reply.child == x11rb::NONE {
            return;
        }
        if let Ok(cookie) = self
            .conn
            .set_input_focus(InputFocus::PARENT, reply.child, CURRENT_TIME)
        {
            cookie.ignore_error();
        }
        let _ = self.conn.flush();
    }
}

/// The X keysym that produces `ch`.
///
/// Latin-1 is the one range where keysyms and Unicode agree by
/// historical accident, so it passes through. Everything else uses the
/// Unicode escape range the X protocol reserves for exactly this. The
/// few control characters worth typing have named keysyms and no
/// printable form, so they are spelled out.
fn keysym_for(ch: char) -> u32 {
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
    Motion {
        x: f64,
        y: f64,
        ack: mpsc::Sender<EngineResult<()>>,
    },
    Button {
        code: u32,
        pressed: bool,
        ack: mpsc::Sender<EngineResult<()>>,
    },
    Keysym {
        keysym: i32,
        pressed: bool,
        ack: mpsc::Sender<EngineResult<()>>,
    },
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
                    let Ok(proxy) = RemoteDesktop::new().await else {
                        let _ = init_tx.send(None);
                        return;
                    };
                    let Ok(session): Result<Session<'_, RemoteDesktop>, _> =
                        proxy.create_session().await
                    else {
                        let _ = init_tx.send(None);
                        return;
                    };
                    // Ask for a keyboard as well as a pointer. The
                    // portal grants devices per session, so a session
                    // opened for the pointer alone cannot type later.
                    let types: BitFlags<DeviceType> = DeviceType::Pointer | DeviceType::Keyboard;
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
                            LibeiCmd::Keysym {
                                keysym,
                                pressed,
                                ack,
                            } => {
                                let state = if pressed {
                                    ashpd::desktop::remote_desktop::KeyState::Pressed
                                } else {
                                    ashpd::desktop::remote_desktop::KeyState::Released
                                };
                                // The portal takes a keysym directly,
                                // so the compositor does the keymap
                                // work the XTest path has to do by
                                // hand.
                                let r = proxy
                                    .notify_keyboard_keysym(&session, keysym, state)
                                    .await
                                    .map_err(|e| EngineError::Other(format!("ei keysym: {e}")));
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

    fn key_down(&self, ch: char, _settle: &dyn Fn(Duration)) -> EngineResult<()> {
        self.keysym(ch, true)
    }

    fn key_up(&self, ch: char) -> EngineResult<()> {
        self.keysym(ch, false)
    }
}

impl LibeiDispatcher {
    fn keysym(&self, ch: char, pressed: bool) -> EngineResult<()> {
        let (ack_tx, ack_rx) = mpsc::channel();
        #[allow(clippy::cast_possible_wrap)]
        let keysym = keysym_for(ch) as i32;
        self.cmd_tx
            .send(LibeiCmd::Keysym {
                keysym,
                pressed,
                ack: ack_tx,
            })
            .map_err(|_| EngineError::Other("libei worker thread gone".into()))?;
        ack_rx
            .recv()
            .map_err(|_| EngineError::Other("libei ack channel closed".into()))?
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
