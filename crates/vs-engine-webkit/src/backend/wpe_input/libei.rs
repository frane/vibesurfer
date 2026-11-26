//! libei dispatcher through the xdg-desktop-portal `RemoteDesktop`
//! interface (`ashpd`), for pure-Wayland sessions.
//!
//! We don't talk to libei directly — `ashpd`'s `RemoteDesktop` portal
//! exposes `notify_pointer_motion_absolute` and `notify_pointer_button`
//! as D-Bus methods, which the compositor delivers to focused windows
//! as trusted hardware input. This is the recommended path for
//! non-interactive applications on modern Wayland sessions (GNOME 41+,
//! KDE Plasma 5.27+, sway via wlroots-virtual-pointer).
//!
//! We park a current-thread tokio runtime on a dedicated thread, run
//! the portal session there, and `block_on` for every `dispatch` call.
//! The portal session lifecycle (create_session → select_devices →
//! start) happens once at process startup; the user sees a one-time
//! permission prompt from their compositor. Successive `dispatch`
//! calls are cheap D-Bus method calls.

use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop};
use ashpd::desktop::{PersistMode, Session};
use enumflags2::BitFlags;
use std::sync::mpsc;
use std::time::Duration;

use crate::engine::{EngineError, EngineResult};

use super::{keysym_for, Button, InputDispatcher, InputEvent};

/// libei (xdg-desktop-portal RemoteDesktop) dispatcher.
pub(super) struct LibeiDispatcher {
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
    Axis {
        dx: f64,
        dy: f64,
        ack: mpsc::Sender<EngineResult<()>>,
    },
}

impl LibeiDispatcher {
    pub(super) fn try_new() -> Option<Self> {
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
                            LibeiCmd::Axis { dx, dy, ack } => {
                                let r = proxy
                                    .notify_pointer_axis(&session, dx, dy, true)
                                    .await
                                    .map_err(|e| EngineError::Other(format!("ei axis: {e}")));
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
            InputEvent::Wheel { dx, dy, .. } => LibeiCmd::Axis {
                dx,
                dy,
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

    fn modifier(&self, keysym: u32, pressed: bool) -> EngineResult<()> {
        let (ack_tx, ack_rx) = mpsc::channel();
        #[allow(clippy::cast_possible_wrap)]
        self.cmd_tx
            .send(LibeiCmd::Keysym {
                keysym: keysym as i32,
                pressed,
                ack: ack_tx,
            })
            .map_err(|_| EngineError::Other("libei worker thread gone".into()))?;
        ack_rx
            .recv()
            .map_err(|_| EngineError::Other("libei ack channel closed".into()))?
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
