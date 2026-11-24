//! A drop from another application on X11: vibesurfer plays the drag
//! source of the XDND protocol against the page's window. It owns
//! `XdndSelection`, sends `XdndEnter` and `XdndPosition` until the page
//! answers with `XdndStatus`, then `XdndDrop`, serves the data GTK asks
//! for, and waits for `XdndFinished`. The source runs on a thread with
//! its own connection, since GTK answers from the main loop, which the
//! caller keeps turning.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    SelectionNotifyEvent, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

use crate::engine::{DropData, EngineError, EngineResult};

const VERSION: u32 = 5;

fn err(what: &str, e: impl std::fmt::Display) -> EngineError {
    EngineError::Other(format!("drop: {what}: {e}"))
}

/// The drag's data as (MIME type, bytes): files as a `text/uri-list` of
/// `file://` URIs; without files, the string items GTK can read.
fn offers(data: &DropData) -> Vec<(String, Vec<u8>)> {
    if !data.files.is_empty() {
        let mut uris = String::new();
        for f in &data.files {
            let p = std::fs::canonicalize(f).unwrap_or_else(|_| f.clone());
            uris.push_str("file://");
            uris.push_str(&percent_encode(&p.to_string_lossy()));
            uris.push_str("\r\n");
        }
        // With files the page sees only `Files`; strings go by script.
        return vec![("text/uri-list".into(), uris.into_bytes())];
    }
    let mut out = Vec::new();
    for (mime, value) in &data.items {
        match mime.to_ascii_lowercase().as_str() {
            "text/plain" => {
                for ty in ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"] {
                    out.push((ty.to_string(), value.clone().into_bytes()));
                }
            }
            "text/html" => out.push(("text/html".into(), value.clone().into_bytes())),
            "text/uri-list" => out.push((
                "text/uri-list".into(),
                value.replace('\n', "\r\n").into_bytes(),
            )),
            _ => {}
        }
    }
    out
}

fn percent_encode(path: &str) -> String {
    let mut out = String::new();
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("%{b:02X}"));
        }
    }
    out
}

struct Atoms {
    selection: u32,
    aware: u32,
    enter: u32,
    position: u32,
    status: u32,
    drop: u32,
    leave: u32,
    finished: u32,
    copy: u32,
    type_list: u32,
    targets: u32,
}

fn intern(conn: &RustConnection, name: &str) -> EngineResult<u32> {
    Ok(conn
        .intern_atom(false, name.as_bytes())
        .map_err(|e| err("intern_atom", e))?
        .reply()
        .map_err(|e| err("intern_atom", e))?
        .atom)
}

fn send(conn: &RustConnection, to: u32, kind: u32, data: [u32; 5]) -> EngineResult<()> {
    let ev = ClientMessageEvent::new(32, to, kind, data);
    conn.send_event(false, to, EventMask::NO_EVENT, ev)
        .map_err(|e| err("send_event", e))?;
    conn.flush().map_err(|e| err("flush", e))
}

/// One drag in progress: our source window, offering `offers` to `target`.
struct Source {
    conn: RustConnection,
    atoms: Atoms,
    offers: Vec<(u32, Vec<u8>)>,
    window: u32,
    target: u32,
}

impl Source {
    /// Connect, check `target` takes drops, and own the XDND selection.
    fn open(target: u32, data: &DropData) -> EngineResult<Self> {
        let (conn, screen) = RustConnection::connect(None).map_err(|e| err("connect", e))?;
        let root = conn.setup().roots[screen].root;
        let atoms = Atoms {
            selection: intern(&conn, "XdndSelection")?,
            aware: intern(&conn, "XdndAware")?,
            enter: intern(&conn, "XdndEnter")?,
            position: intern(&conn, "XdndPosition")?,
            status: intern(&conn, "XdndStatus")?,
            drop: intern(&conn, "XdndDrop")?,
            leave: intern(&conn, "XdndLeave")?,
            finished: intern(&conn, "XdndFinished")?,
            copy: intern(&conn, "XdndActionCopy")?,
            type_list: intern(&conn, "XdndTypeList")?,
            targets: intern(&conn, "TARGETS")?,
        };
        let offers = offers(data)
            .into_iter()
            .map(|(ty, bytes)| intern(&conn, &ty).map(|atom| (atom, bytes)))
            .collect::<EngineResult<_>>()?;
        let aware = conn
            .get_property(false, target, atoms.aware, AtomEnum::ATOM, 0, 1)
            .map_err(|e| err("get_property", e))?
            .reply()
            .map_err(|e| err("get_property", e))?;
        if aware.value_len == 0 {
            return Err(err("target window", "does not accept drops (no XdndAware)"));
        }
        let window = conn.generate_id().map_err(|e| err("generate_id", e))?;
        conn.create_window(
            0,
            window,
            root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )
        .map_err(|e| err("create_window", e))?;
        conn.set_selection_owner(window, atoms.selection, CURRENT_TIME)
            .map_err(|e| err("set_selection_owner", e))?;
        Ok(Self {
            conn,
            atoms,
            offers,
            window,
            target,
        })
    }

    fn send(&self, kind: u32, data: [u32; 5]) -> EngineResult<()> {
        send(&self.conn, self.target, kind, data)
    }

    /// Serve any data request that arrives within `wait`.
    fn serve_for(&self, wait: Duration) -> EngineResult<()> {
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if let Some(Event::SelectionRequest(r)) =
                next_event(&self.conn, Duration::from_millis(10))?
            {
                serve(&self.conn, &self.atoms, &self.offers, &r)?;
            }
        }
        Ok(())
    }

    /// `XdndEnter` with our types (all of them in `XdndTypeList`).
    fn enter(&self) -> EngineResult<()> {
        let types: Vec<u32> = self.offers.iter().map(|o| o.0).collect();
        self.conn
            .change_property32(
                PropMode::REPLACE,
                self.window,
                self.atoms.type_list,
                AtomEnum::ATOM,
                &types,
            )
            .map_err(|e| err("change_property", e))?;
        let type_at = |i: usize| types.get(i).copied().unwrap_or(0);
        let more = u32::from(types.len() > 3);
        self.send(
            self.atoms.enter,
            [
                self.window,
                (VERSION << 24) | more,
                type_at(0),
                type_at(1),
                type_at(2),
            ],
        )
    }

    /// Hover over root point `at` (x << 16 | y) for a few rounds and
    /// return whether the page's last answer accepted the drop. GTK
    /// accepts at once but reads the data first, and only then hands
    /// WebKit the dragover that decides.
    fn hover(&self, at: u32) -> EngineResult<bool> {
        let mut accepted = false;
        for _ in 0..8 {
            self.send(
                self.atoms.position,
                [self.window, 0, at, CURRENT_TIME, self.atoms.copy],
            )?;
            if let Some(ok) = wait_status(
                &self.conn,
                &self.atoms,
                &self.offers,
                self.window,
                Duration::from_millis(400),
            )? {
                accepted = ok;
            }
            self.serve_for(Duration::from_millis(40))?;
        }
        Ok(accepted)
    }

    fn leave(&self) -> EngineResult<()> {
        self.send(self.atoms.leave, [self.window, 0, 0, 0, 0])
    }

    /// `XdndDrop`, then serve the page's reads until `XdndFinished`.
    fn drop_here(&self) -> EngineResult<()> {
        self.send(self.atoms.drop, [self.window, 0, CURRENT_TIME, 0, 0])?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match next_event(&self.conn, Duration::from_millis(50))? {
                Some(Event::ClientMessage(m)) if m.type_ == self.atoms.finished => return Ok(()),
                Some(Event::SelectionRequest(r)) => {
                    serve(&self.conn, &self.atoms, &self.offers, &r)?;
                }
                _ => {}
            }
        }
        Err(err(
            "XdndFinished",
            "the page did not finish the drop in 5 s",
        ))
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.conn.destroy_window(self.window);
        let _ = self.conn.flush();
    }
}

/// Run the drag against `target` at root point (`root_x`, `root_y`).
/// Returns whether the page accepted it.
fn run(target: u32, root_x: u16, root_y: u16, data: &DropData) -> EngineResult<bool> {
    let source = Source::open(target, data)?;
    source.enter()?;
    if !source.hover((u32::from(root_x) << 16) | u32::from(root_y))? {
        source.leave()?;
        return Ok(false);
    }
    source.drop_here()?;
    Ok(true)
}

fn next_event(conn: &RustConnection, wait: Duration) -> EngineResult<Option<Event>> {
    let until = Instant::now() + wait;
    loop {
        if let Some(ev) = conn
            .poll_for_event()
            .map_err(|e| err("poll_for_event", e))?
        {
            return Ok(Some(ev));
        }
        if Instant::now() >= until {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The next `XdndStatus`: `Some(accepted)`, or `None` on timeout.
/// Serves selection requests that arrive meanwhile.
fn wait_status(
    conn: &RustConnection,
    atoms: &Atoms,
    offers: &[(u32, Vec<u8>)],
    source: u32,
    wait: Duration,
) -> EngineResult<Option<bool>> {
    let until = Instant::now() + wait;
    while Instant::now() < until {
        match next_event(conn, Duration::from_millis(20))? {
            Some(Event::ClientMessage(m)) if m.type_ == atoms.status && m.window == source => {
                let d = m.data.as_data32();
                return Ok(Some(d[1] & 1 == 1));
            }
            Some(Event::SelectionRequest(r)) => serve(conn, atoms, offers, &r)?,
            _ => {}
        }
    }
    Ok(None)
}

/// Answer a `ConvertSelection` from the page: the bytes of the asked
/// type (or the list of types for `TARGETS`) on the requestor's
/// property, then `SelectionNotify`.
fn serve(
    conn: &RustConnection,
    atoms: &Atoms,
    offers: &[(u32, Vec<u8>)],
    r: &x11rb::protocol::xproto::SelectionRequestEvent,
) -> EngineResult<()> {
    let property = if r.property == 0 {
        r.target
    } else {
        r.property
    };
    let mut ok = true;
    if r.target == atoms.targets {
        let mut list: Vec<u32> = offers.iter().map(|o| o.0).collect();
        list.push(atoms.targets);
        conn.change_property32(
            PropMode::REPLACE,
            r.requestor,
            property,
            AtomEnum::ATOM,
            &list,
        )
        .map_err(|e| err("change_property", e))?;
    } else if let Some((ty, bytes)) = offers.iter().find(|o| o.0 == r.target) {
        conn.change_property8(PropMode::REPLACE, r.requestor, property, *ty, bytes)
            .map_err(|e| err("change_property", e))?;
    } else {
        ok = false;
    }
    let ev = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: r.time,
        requestor: r.requestor,
        selection: r.selection,
        target: r.target,
        property: if ok { property } else { 0 },
    };
    conn.send_event(false, r.requestor, EventMask::NO_EVENT, ev)
        .map_err(|e| err("send_event", e))?;
    conn.flush().map_err(|e| err("flush", e))
}

/// Drop `data` on X window `target` at root point (`x`, `y`), turning
/// the GTK main loop with `turn` until the drag ends.
pub(super) fn drop_data(
    target: u32,
    x: f64,
    y: f64,
    data: &DropData,
    mut turn: impl FnMut(Duration),
) -> EngineResult<bool> {
    let (tx, rx) = mpsc::channel();
    let data = data.clone();
    // Root coordinates are 16-bit in XDND messages.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (x, y) = (
        x.round().clamp(0.0, f64::from(u16::MAX)) as u16,
        y.round().clamp(0.0, f64::from(u16::MAX)) as u16,
    );
    std::thread::Builder::new()
        .name("vs-xdnd".into())
        .spawn(move || {
            let _ = tx.send(run(target, x, y, &data));
        })
        .map_err(|e| err("spawn", e))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(r) = rx.try_recv() {
            // Let the page run its drop handlers.
            turn(Duration::from_millis(100));
            return r;
        }
        if Instant::now() >= deadline {
            return Err(err("XDND", "no answer in 10 s"));
        }
        turn(Duration::from_millis(10));
    }
}
