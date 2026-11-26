//! XTest dispatcher: `FakeInput` through the pure-Rust `x11rb` client.
//! Works under any X11 or Xwayland session, including `xvfb` in CI.

use std::sync::Mutex;
use std::time::Duration;

use x11rb::connection::Connection as _;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt as _, InputFocus, StackMode, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

use crate::engine::{EngineError, EngineResult};

use super::{keysym_for, Button, InputDispatcher, InputEvent, ScreenPoint};

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
pub(super) struct XtestDispatcher {
    conn: RustConnection,
    root: Window,
    /// The keycode borrowed from the server's keymap for typing, and
    /// the keysym currently bound to it. See [`XtestDispatcher::bind`].
    scratch: Mutex<Option<ScratchKey>>,
    /// Page windows found by title (see `find_titled`).
    windows: Mutex<std::collections::HashMap<String, Window>>,
    /// `_NET_WM_NAME` and `UTF8_STRING`, interned once.
    name_atoms: (u32, u32),
    /// Whether the server speaks XInput 2.2 (multiple pointers).
    xi2: bool,
    /// Each page's own master pointer, by window title, and the ones
    /// this daemon holds but no page uses (see `page_pointer`).
    pointers: Mutex<PointerPool>,
    /// A window of ours that owns the `VS_PTR_<k>` selections: the
    /// claim on pointer k, released by the server when we exit.
    claim_window: Window,
}

#[derive(Default)]
struct PointerPool {
    assigned: std::collections::HashMap<String, u16>,
    free: Vec<u16>,
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
    pub(super) fn try_new() -> Option<Self> {
        // `connect(None)` reads `$DISPLAY`. The first roots[0] is the
        // default screen — same convention every X11 client uses.
        let (conn, screen_num) = RustConnection::connect(None).ok()?;
        let root = conn.setup().roots.get(screen_num)?.root;
        let atom = |name: &[u8]| -> Option<u32> {
            Some(conn.intern_atom(false, name).ok()?.reply().ok()?.atom)
        };
        let name_atoms = (atom(b"_NET_WM_NAME")?, atom(b"UTF8_STRING")?);
        // XI2 requests need the version handshake first on a client.
        let xi2 = conn
            .xinput_xi_query_version(2, 2)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| (r.major_version, r.minor_version) >= (2, 2));
        let claim_window = conn.generate_id().ok()?;
        conn.create_window(
            0,
            claim_window,
            root,
            0,
            0,
            1,
            1,
            0,
            x11rb::protocol::xproto::WindowClass::INPUT_ONLY,
            0,
            &x11rb::protocol::xproto::CreateWindowAux::new(),
        )
        .ok()?;
        let me = Self {
            conn,
            root,
            scratch: Mutex::new(None),
            windows: Mutex::default(),
            name_atoms,
            xi2,
            pointers: Mutex::default(),
            claim_window,
        };
        if xi2 {
            me.park_unclaimed_pointers();
        }
        Some(me)
    }

    /// Move every `vs-ptr-<k>` pointer no live daemon claims to the
    /// screen's far corner. A daemon that exited left its pointers
    /// where its pages' input put them, and one resting inside a tile
    /// would hover whichever page goes there next.
    fn park_unclaimed_pointers(&self) {
        let Some(devices) = self
            .conn
            .xinput_xi_query_device(xinput::Device::ALL_MASTER)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return;
        };
        for d in devices
            .infos
            .iter()
            .filter(|d| d.type_ == xinput::DeviceType::MASTER_POINTER)
        {
            let Some(k) = std::str::from_utf8(&d.name)
                .ok()
                .and_then(|n| n.strip_prefix("vs-ptr-"))
                .and_then(|n| n.strip_suffix(" pointer"))
            else {
                continue;
            };
            let claimed = self
                .conn
                .intern_atom(false, format!("VS_PTR_{k}").as_bytes())
                .ok()
                .and_then(|c| c.reply().ok())
                .and_then(|a| self.conn.get_selection_owner(a.atom).ok())
                .and_then(|c| c.reply().ok())
                .is_some_and(|r| r.owner != x11rb::NONE);
            if !claimed {
                self.park(d.deviceid);
            }
        }
        self.use_core_pointer();
    }

    /// Move master pointer `id` to the screen's far corner, past every
    /// page window and tile.
    fn park(&self, id: u16) {
        let Some((w, h)) = self.screen_size() else {
            return;
        };
        if let Ok(cookie) = self.conn.xinput_xi_set_client_pointer(x11rb::NONE, id) {
            cookie.ignore_error();
        }
        let _ = self.dispatch(InputEvent::Move(ScreenPoint { x: w - 1, y: h - 1 }));
        let _ = self.conn.sync();
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

impl XtestDispatcher {
    /// The keycode the server's keymap already has for `keysym`.
    fn keycode_of(&self, keysym: u32) -> EngineResult<Option<u8>> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let map = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)
            .map_err(|e| EngineError::Other(format!("GetKeyboardMapping: {e}")))?
            .reply()
            .map_err(|e| EngineError::Other(format!("GetKeyboardMapping reply: {e}")))?;
        let per = usize::from(map.keysyms_per_keycode.max(1));
        Ok(map
            .keysyms
            .chunks(per)
            .position(|syms| syms.contains(&keysym))
            .and_then(|i| u8::try_from(i).ok())
            .map(|i| min + i))
    }

    /// A wheel turn as clicks of the X11 wheel buttons (4/5 vertical,
    /// 6/7 horizontal): XTest has no smooth-scroll axis, so the turn is
    /// rounded to whole clicks of `step` px, at least one for any
    /// nonzero delta.
    fn wheel_clicks(&self, dx: f64, dy: f64, step: (f64, f64)) -> EngineResult<()> {
        let clicks = |d: f64, step: f64| -> u32 {
            if d == 0.0 {
                0
            } else {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let n = (d.abs() / step.max(1.0)).round() as u32;
                n.max(1)
            }
        };
        let axes = [
            (if dy > 0.0 { 5_u8 } else { 4 }, clicks(dy, step.1)),
            (if dx > 0.0 { 7_u8 } else { 6 }, clicks(dx, step.0)),
        ];
        for (button, n) in axes {
            for _ in 0..n {
                for type_ in [XT_BUTTON_PRESS, XT_BUTTON_RELEASE] {
                    self.conn
                        .xtest_fake_input(type_, button, CURRENT_TIME, self.root, 0, 0, 0)
                        .map_err(|e| EngineError::Other(format!("xtest wheel: {e}")))?
                        .ignore_error();
                }
            }
        }
        Ok(())
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
            InputEvent::Wheel { dx, dy, step } => return self.wheel_clicks(dx, dy, step),
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

    fn modifier(&self, keysym: u32, pressed: bool) -> EngineResult<()> {
        let keycode = match self.keycode_of(keysym)? {
            Some(k) => k,
            None => self.bind(keysym, &|d| std::thread::sleep(d))?,
        };
        let type_ = if pressed {
            XT_KEY_PRESS
        } else {
            XT_KEY_RELEASE
        };
        self.conn
            .xtest_fake_input(type_, keycode, CURRENT_TIME, self.root, 0, 0, 0)
            .map_err(|e| EngineError::Other(format!("xtest modifier: {e}")))?
            .ignore_error();
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

    fn raise_window(&self, title: &str) {
        // Without a window manager every host window is a direct child
        // of the root, stacked in the order it was mapped. Find ours by
        // its _NET_WM_NAME (GTK sets it from the title) and restack it.
        let Some(win) = self.find_titled(title) else {
            return;
        };
        let aux = ConfigureWindowAux::new().stack_mode(StackMode::ABOVE);
        if let Ok(cookie) = self.conn.configure_window(win, &aux) {
            cookie.ignore_error();
        }
        let _ = self.conn.sync();
    }

    fn screen_size(&self) -> Option<(i32, i32)> {
        let screen = self.conn.setup().roots.first()?;
        Some((
            i32::from(screen.width_in_pixels),
            i32::from(screen.height_in_pixels),
        ))
    }

    fn place_window(&self, title: &str, x: i32, y: i32) -> bool {
        let Some(win) = self.find_titled(title) else {
            return false;
        };
        let aux = ConfigureWindowAux::new().x(x).y(y);
        if let Ok(cookie) = self.conn.configure_window(win, &aux) {
            cookie.ignore_error();
        }
        self.conn.sync().is_ok()
    }

    fn x11_window(&self, title: &str) -> Option<u32> {
        self.find_titled(title)
    }

    fn window_origin(&self, title: &str) -> Option<(i32, i32)> {
        let win = self.find_titled(title)?;
        let r = self
            .conn
            .translate_coordinates(win, self.root, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        Some((i32::from(r.dst_x), i32::from(r.dst_y)))
    }

    fn use_page_pointer(&self, title: &str) {
        // XTest's core events move the pointer (and type on the
        // keyboard) of the client's ClientPointer, so pointing this
        // connection at the page's own master pair routes everything
        // after it there: its own position, hover state and focus.
        let Some(id) = self.page_pointer(title) else {
            return;
        };
        if let Ok(cookie) = self.conn.xinput_xi_set_client_pointer(x11rb::NONE, id) {
            cookie.ignore_error();
        }
        let _ = self.conn.sync();
    }

    fn use_core_pointer(&self) {
        if !self.xi2 {
            return;
        }
        // Device 2 is always the core ("Virtual core") pointer.
        if let Ok(cookie) = self.conn.xinput_xi_set_client_pointer(x11rb::NONE, 2_u16) {
            cookie.ignore_error();
        }
        let _ = self.conn.sync();
    }

    fn drop_page_pointer(&self, title: &str) {
        // Kept, not removed: removing a master device while GTK runs
        // crashes it. Parked out of the way; the next page this daemon
        // opens takes it.
        let freed = self.pointers.lock().ok().and_then(|mut pool| {
            let id = pool.assigned.remove(title)?;
            pool.free.push(id);
            Some(id)
        });
        if let Some(id) = freed {
            self.park(id);
            self.use_core_pointer();
        }
    }
}

impl XtestDispatcher {
    /// The page's own master pointer: the one it already has, one this
    /// daemon holds free, or a claimed one. Pointer k is named
    /// `vs-ptr-<k>` and claimed by owning the `VS_PTR_<k>` selection;
    /// the server drops the claim when the daemon exits, so a later
    /// daemon reuses the device instead of adding another (devices are
    /// never removed; see `drop_page_pointer`).
    fn page_pointer(&self, title: &str) -> Option<u16> {
        if !self.xi2 {
            return None;
        }
        let mut pool = self.pointers.lock().ok()?;
        if let Some(&id) = pool.assigned.get(title) {
            return Some(id);
        }
        let id = match pool.free.pop() {
            Some(id) => id,
            None => self.claim_pointer()?,
        };
        pool.assigned.insert(title.to_string(), id);
        Some(id)
    }

    /// Claim the first pointer no live daemon holds, making it if it
    /// does not exist yet.
    fn claim_pointer(&self) -> Option<u16> {
        for k in 0..256 {
            let atom = self
                .conn
                .intern_atom(false, format!("VS_PTR_{k}").as_bytes())
                .ok()?
                .reply()
                .ok()?
                .atom;
            let owner = self
                .conn
                .get_selection_owner(atom)
                .ok()?
                .reply()
                .ok()?
                .owner;
            if owner != x11rb::NONE {
                continue;
            }
            self.conn
                .set_selection_owner(self.claim_window, atom, CURRENT_TIME)
                .ok()?;
            // Another daemon may have claimed it in between.
            let owner = self
                .conn
                .get_selection_owner(atom)
                .ok()?
                .reply()
                .ok()?
                .owner;
            if owner != self.claim_window {
                continue;
            }
            let name = format!("vs-ptr-{k}");
            return self.master_named(&name).or_else(|| self.add_master(&name));
        }
        None
    }

    /// The id of the master pointer `<name> pointer`, if it exists.
    fn master_named(&self, name: &str) -> Option<u16> {
        let want = format!("{name} pointer");
        let devices = self
            .conn
            .xinput_xi_query_device(xinput::Device::ALL_MASTER)
            .ok()?
            .reply()
            .ok()?;
        devices
            .infos
            .iter()
            .find(|d| d.type_ == xinput::DeviceType::MASTER_POINTER && d.name == want.as_bytes())
            .map(|d| d.deviceid)
    }

    /// Add a master pointer and keyboard pair named `name`.
    fn add_master(&self, title: &str) -> Option<u16> {
        let name = title.as_bytes().to_vec();
        // Length in 4-byte units: an 8-byte header, then the name
        // padded to a multiple of four.
        let len = u16::try_from((8 + name.len().div_ceil(4) * 4) / 4).ok()?;
        let change = xinput::HierarchyChange {
            len,
            data: xinput::HierarchyChangeData::AddMaster(xinput::HierarchyChangeDataAddMaster {
                send_core: true,
                enable: true,
                name,
            }),
        };
        self.conn
            .xinput_xi_change_hierarchy(&[change])
            .ok()?
            .check()
            .ok()?;
        // The server names the pair "<name> pointer" / "<name> keyboard".
        self.master_named(title)
    }
}

impl XtestDispatcher {
    /// The top-level window titled `title`. Remembered once found: a
    /// lookup walks every top-level window's name, a round trip each.
    fn find_titled(&self, title: &str) -> Option<Window> {
        let cached = self.windows.lock().ok()?.get(title).copied();
        // Still that window: a closed page's id may since name another.
        if let Some(w) = cached.filter(|&w| self.titled_as(w, title)) {
            return Some(w);
        }
        let found = self.scan_titled(title)?;
        if let Ok(mut m) = self.windows.lock() {
            m.insert(title.to_string(), found);
        }
        Some(found)
    }

    fn scan_titled(&self, title: &str) -> Option<Window> {
        let tree = self.conn.query_tree(self.root).ok()?.reply().ok()?;
        tree.children
            .into_iter()
            .rev()
            .find(|&w| self.titled_as(w, title))
    }

    /// Whether window `w` is named `title` (_NET_WM_NAME, which GTK sets
    /// from the title, else WM_NAME).
    fn titled_as(&self, w: Window, title: &str) -> bool {
        let (net_wm_name, utf8) = self.name_atoms;
        let name = |prop, ty| {
            self.conn
                .get_property(false, w, prop, ty, 0, 256)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| r.value)
        };
        name(net_wm_name, utf8)
            .filter(|v| !v.is_empty())
            .or_else(|| name(AtomEnum::WM_NAME.into(), AtomEnum::STRING.into()))
            .is_some_and(|v| v == title.as_bytes())
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
