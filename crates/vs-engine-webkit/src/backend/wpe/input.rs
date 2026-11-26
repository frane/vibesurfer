//! Native input on the X display (XTest or the RemoteDesktop portal):
//! clicks, humanized pointer paths, where a page's window sits, and
//! delivering queued input to it.

use std::time::Duration;

use gtk4 as gtk;
use webkit6::prelude::*;

use super::run_loop_until;
use crate::engine::{EngineResult, PageHandle};

/// Press and release `opts.button` `opts.count` times at the pointer,
/// with the modifier keys held around the clicks. Clicks this close
/// together are one double (or triple) click to GTK.
pub(super) fn click_with(
    dispatcher: &dyn crate::backend::wpe_input::InputDispatcher,
    opts: crate::engine::ClickOpts,
) -> EngineResult<()> {
    use crate::backend::wpe_input::{Button, InputEvent};
    use crate::engine::MouseButton;
    // Shift_L, Control_L, Alt_L, Super_L.
    let held: Vec<u32> = [
        (opts.modifiers.shift, 0xffe1),
        (opts.modifiers.ctrl, 0xffe3),
        (opts.modifiers.alt, 0xffe9),
        (opts.modifiers.meta, 0xffeb),
    ]
    .into_iter()
    .filter_map(|(on, k)| on.then_some(k))
    .collect();
    for &k in &held {
        dispatcher.modifier(k, true)?;
    }
    let button = match opts.button {
        MouseButton::Left => Button::Left,
        MouseButton::Right => Button::Right,
        MouseButton::Middle => Button::Middle,
    };
    for _ in 0..opts.count.max(1) {
        dispatcher.dispatch(InputEvent::Press(button))?;
        dispatcher.dispatch(InputEvent::Release(button))?;
    }
    for &k in held.iter().rev() {
        dispatcher.modifier(k, false)?;
    }
    dispatcher.flush()
}

/// The X window title of a page's host window. XTest input goes by
/// screen position, so the input path finds a page's window by this
/// name to place it in its tile, raise it, and give it its own pointer.
pub(super) fn window_title(page: PageHandle) -> String {
    // The daemon's pid, so two daemons on one display never share a
    // name (and `wpe_input` can tell a dead daemon's leftovers).
    format!("vs-page-{}-{}", std::process::id(), page.0)
}

/// Hand the input just sent to WebKit before returning. XTest events go
/// from the server to GTK's own connection, and GTK handles them on a
/// later main-loop pass; a read of the page sent right after (an eval)
/// could reach the web process first and see the page from before the
/// click. A display sync makes sure GTK has received every event the
/// server generated so far; dispatching what is pending then passes the
/// events to WebKit, whose messages to the web process keep that order.
pub(super) fn deliver_input(
    dispatcher: &dyn crate::backend::wpe_input::InputDispatcher,
    window: &gtk::Window,
) {
    let _ = dispatcher.flush();
    gtk::prelude::WidgetExt::display(window).sync();
    let ctx = glib::MainContext::default();
    for _ in 0..64 {
        if !ctx.iteration(false) {
            break;
        }
    }
}

/// Move the display's core pointer to the screen's far corner, past
/// every page window and tile, and leave input routed through it.
pub(super) fn park_core_pointer(
    dispatcher: &dyn crate::backend::wpe_input::InputDispatcher,
    size: (i32, i32),
) {
    dispatcher.use_core_pointer();
    let _ = dispatcher.dispatch(crate::backend::wpe_input::InputEvent::Move(
        crate::backend::wpe_input::ScreenPoint {
            x: size.0 - 1,
            y: size.1 - 1,
        },
    ));
    let _ = dispatcher.flush();
}

/// The screen origin to add to page coordinates for input. GTK moves a
/// window back to where it thinks it is when it resizes (`vs viewport`),
/// so put a drifted window back into its tile first; otherwise trust
/// where X says the window is.
pub(super) fn input_origin(
    dispatcher: &dyn crate::backend::wpe_input::InputDispatcher,
    page: PageHandle,
    home: (f64, f64),
) -> (f64, f64) {
    let title = window_title(page);
    let Some((x, y)) = dispatcher.window_origin(&title) else {
        return home;
    };
    let now = (f64::from(x), f64::from(y));
    #[allow(clippy::cast_possible_truncation)]
    let moved_back = home != (0.0, 0.0)
        && now != home
        && dispatcher.place_window(&title, home.0 as i32, home.1 as i32);
    if moved_back {
        home
    } else {
        now
    }
}

/// Walk a Bezier path from `start` to `end`, dispatching one `Move`
/// per step at the path's prescribed timing. Returns the final point.
/// The `button_down` flag is unused on Linux today (XTest doesn't
/// distinguish drag-move from plain-move — both are `MotionNotify`
/// events; the held button state is tracked at the X server). Kept
/// in the signature so drag callers stay symmetric with the macOS
/// `move_along_path`.
pub(super) fn cursor_move_along_path(
    dispatcher: &dyn crate::backend::wpe_input::InputDispatcher,
    start: vs_humanize::Point,
    end: vs_humanize::Point,
    origin: (f64, f64),
    mode: vs_humanize::InputMode,
    seed: u64,
    _button_down: bool,
) -> EngineResult<vs_humanize::Point> {
    let path = vs_humanize::mouse_path(start, end, mode, seed);
    let mut prev_ms: u128 = 0;
    for step in &path {
        if step.kind != vs_humanize::MouseStepKind::Move {
            continue;
        }
        let screen = crate::backend::wpe_input::ScreenPoint {
            #[allow(clippy::cast_possible_truncation)]
            x: (origin.0 + step.point.x).round() as i32,
            #[allow(clippy::cast_possible_truncation)]
            y: (origin.1 + step.point.y).round() as i32,
        };
        dispatcher.dispatch(crate::backend::wpe_input::InputEvent::Move(screen))?;
        let dt = step.at.as_millis().saturating_sub(prev_ms);
        if dt > 0 {
            std::thread::sleep(Duration::from_millis(u64::try_from(dt).unwrap_or(0)));
        }
        prev_ms = step.at.as_millis();
    }
    // Final settling move ending exactly at `end`.
    let final_pt = crate::backend::wpe_input::ScreenPoint {
        #[allow(clippy::cast_possible_truncation)]
        x: (origin.0 + end.x).round() as i32,
        #[allow(clippy::cast_possible_truncation)]
        y: (origin.1 + end.y).round() as i32,
    };
    dispatcher.dispatch(crate::backend::wpe_input::InputEvent::Move(final_pt))?;
    dispatcher.flush()?;
    Ok(end)
}

/// Press-then-release left button at the current pointer position.
/// Same cadence as the macOS backend (robotic 2 ms / 6 ms, otherwise
/// 15 ms / 30 ms), so JS sees a coherent `click` event.
pub(super) fn cursor_press_release(
    dispatcher: &dyn crate::backend::wpe_input::InputDispatcher,
    _at: vs_humanize::Point,
    _origin: (f64, f64),
    mode: vs_humanize::InputMode,
) -> EngineResult<()> {
    // Robotic input holds the button briefly, as on macOS; the other
    // modes keep a human press. The waits pump the main loop (a plain
    // sleep stalled GTK, which then saw the events only afterwards).
    let (down_ms, up_ms) = match mode {
        vs_humanize::InputMode::Robotic => (2, 6),
        _ => (15, 30),
    };
    dispatcher.dispatch(crate::backend::wpe_input::InputEvent::Press(
        crate::backend::wpe_input::Button::Left,
    ))?;
    dispatcher.flush()?;
    let _ = run_loop_until(|| false, Duration::from_millis(down_ms));
    dispatcher.dispatch(crate::backend::wpe_input::InputEvent::Release(
        crate::backend::wpe_input::Button::Left,
    ))?;
    dispatcher.flush()?;
    let _ = run_loop_until(|| false, Duration::from_millis(up_ms));
    Ok(())
}

/// How long the left button is held at a drag's start before the move,
/// so the page sees a press, not a click.
const DRAG_PRESS_SETTLE: Duration = Duration::from_millis(15);

/// Press the left button, run `move_while_held`, release, and flush.
pub(super) fn drag_from(
    dispatcher: &dyn crate::backend::wpe_input::InputDispatcher,
    move_while_held: impl FnOnce() -> EngineResult<vs_humanize::Point>,
) -> EngineResult<vs_humanize::Point> {
    use crate::backend::wpe_input::{Button, InputEvent};
    dispatcher.dispatch(InputEvent::Press(Button::Left))?;
    std::thread::sleep(DRAG_PRESS_SETTLE);
    let landed = move_while_held()?;
    dispatcher.dispatch(InputEvent::Release(Button::Left))?;
    dispatcher.flush()?;
    Ok(landed)
}

/// Turn the wheel by `(dx, dy)` at the pointer. Under XTest that is one
/// click WebKit's handler takes as the exact turn (see `wpe_wheel`),
/// with the XTest step as the fallback when that hook is not in place;
/// the portal's axis events are exact already.
pub(super) fn wheel(
    dispatcher: &dyn crate::backend::wpe_input::InputDispatcher,
    web_view: &webkit6::WebView,
    window: &gtk::Window,
    dx: f64,
    dy: f64,
) -> EngineResult<()> {
    use crate::backend::wpe_input::InputEvent;
    let event = if dispatcher.backend_name() == "xtest" {
        crate::backend::wpe_wheel::arm(dx, dy, (web_view.width(), web_view.height()));
        InputEvent::Wheel {
            dx: if dy == 0.0 { dx.signum() } else { 0.0 },
            dy: dy.signum(),
            step: (1.0, 1.0),
        }
    } else {
        InputEvent::Wheel {
            dx,
            dy,
            step: (1.0, 1.0),
        }
    };
    dispatcher.dispatch(event)?;
    dispatcher.flush()?;
    deliver_input(dispatcher, window);
    crate::backend::wpe_wheel::disarm();
    Ok(())
}

/// How long the last key gets to become `beforeinput`/`input` before
/// the caller reads the page back.
const TYPING_SETTLE: Duration = Duration::from_millis(150);

/// Type `text` into whatever holds keyboard focus, as real key events
/// through the core keyboard, with the pointer first put back where the
/// caller left it over page `page` (`cursor`, page coordinates; `tile`,
/// the page window's home on the screen).
pub(super) fn type_keys(
    page: PageHandle,
    cursor: vs_humanize::Point,
    tile: (f64, f64),
    text: &str,
    mode: crate::engine::InputMode,
) -> EngineResult<()> {
    let dispatcher = crate::backend::wpe_input::dispatcher()?;
    let origin = input_origin(dispatcher, page, tile);
    // Pump GLib rather than sleep: this runs on the main thread,
    // so a plain sleep would stop WebKit processing the very keys
    // we are sending.
    let settle = |d: Duration| {
        let _ = run_loop_until(|| false, d);
    };
    // Put the pointer where we believe it already is before asking
    // for focus. X hands keys to the focused window, and a
    // headless session has no window manager to focus anything, so
    // the pointer is what we have to go on. The caller has
    // normally just clicked to place the caret, which means the
    // right window is already under it.
    #[allow(clippy::cast_possible_truncation)]
    let at = crate::backend::wpe_input::ScreenPoint {
        x: (origin.0 + cursor.x) as i32,
        y: (origin.1 + cursor.y) as i32,
    };
    dispatcher.raise_window(&window_title(page));
    // Keys go through the core keyboard (toolkits take them from it
    // only), focused on this page's window by the core pointer.
    dispatcher.use_core_pointer();
    let _ = dispatcher.dispatch(crate::backend::wpe_input::InputEvent::Move(at));
    let _ = dispatcher.flush();
    dispatcher.focus_pointer_window();

    let mut cadence = crate::backend::common::KeyCadence::new(mode.into(), text);

    let typed = (|| -> EngineResult<()> {
        for ch in text.chars() {
            dispatcher.key_down(ch, &settle)?;
            settle(Duration::from_millis(cadence.hold_ms()));
            dispatcher.key_up(ch)?;
            settle(Duration::from_millis(cadence.next_wait_ms().unwrap_or(1)));
        }
        Ok(())
    })();

    // Give the borrowed keycode back whether the run finished or
    // failed halfway: a stray binding would change what the user's
    // own keyboard does for the rest of the session.
    let restored = dispatcher.end_typing();
    typed?;
    restored?;

    let _ = run_loop_until(|| false, TYPING_SETTLE);
    // The core pointer must not stay over this page (or end up over
    // the next one placed here) once typing is done.
    if let Some(size) = dispatcher.screen_size() {
        park_core_pointer(dispatcher, size);
    }
    Ok(())
}
