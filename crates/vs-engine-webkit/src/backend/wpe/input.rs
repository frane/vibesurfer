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
