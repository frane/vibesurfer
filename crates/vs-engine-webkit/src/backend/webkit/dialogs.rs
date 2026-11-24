//! JavaScript dialogs (`alert`, `confirm`, `prompt`) for pages with a
//! popup feed. The UI delegate reports the dialog on the feed and keeps
//! WebKit's completion handler; a main-run-loop timer applies the answer
//! a client gives through the feed (from any thread), or dismisses the
//! dialog after `DIALOG_TIMEOUT`.
//!
//! A timer, not a call into the engine: the page's script is blocked
//! while the dialog is up, and the input primitive that opened it is
//! often still waiting on that script. The timer fires inside that
//! wait's run-loop pumping without re-entering the backend.

use std::cell::Cell;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2_foundation::NSTimer;

use crate::engine::{DialogAnswer, PopupFeed, DIALOG_TIMEOUT};

thread_local! {
    /// Dialogs up right now, when the first of them opened, and how
    /// long dialogs were up before that (see [`open_time`]).
    static OPEN: Cell<(u32, Option<Instant>, Duration)> = const { Cell::new((0, None, Duration::ZERO)) };
}

/// Total time any dialog has been up on this thread. A wait that spans
/// a dialog (the script that opened it is blocked until it is answered)
/// adds the difference to its budget, as Playwright's actions wait for
/// dialog handling.
pub(super) fn open_time() -> Duration {
    OPEN.with(|o| {
        let (_, since, total) = o.get();
        total + since.map_or(Duration::ZERO, |s| s.elapsed())
    })
}

fn opened() {
    OPEN.with(|o| {
        let (n, since, total) = o.get();
        o.set((n + 1, since.or_else(|| Some(Instant::now())), total));
    });
}

fn closed() {
    OPEN.with(|o| {
        let (n, since, total) = o.get();
        let n = n.saturating_sub(1);
        if n == 0 {
            o.set((
                0,
                None,
                total + since.map_or(Duration::ZERO, |s| s.elapsed()),
            ));
        } else {
            o.set((n, since, total));
        }
    });
}

/// How often a pending dialog checks for its answer.
const POLL_SECS: f64 = 0.01;

/// Call `finish` once, with the answer to dialog `id`, or `None` when
/// none came in time.
pub(super) fn await_answer(
    feed: Arc<PopupFeed>,
    id: u64,
    finish: Box<dyn Fn(Option<DialogAnswer>)>,
) {
    let started = Instant::now();
    opened();
    let done = Rc::new(Cell::new(false));
    let block = RcBlock::new(move |timer: NonNull<NSTimer>| {
        if done.get() {
            return;
        }
        let answer = feed.take_answer(id);
        if answer.is_none() && started.elapsed() < DIALOG_TIMEOUT {
            return;
        }
        done.set(true);
        closed();
        // SAFETY: the timer passes itself; it is valid for the call.
        unsafe { timer.as_ref() }.invalidate();
        finish(answer);
    });
    // SAFETY: scheduled on the current (main) run loop and fired there
    // only, so the block's main-thread-only captures never cross threads.
    let _ =
        unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(POLL_SECS, true, &block) };
}
