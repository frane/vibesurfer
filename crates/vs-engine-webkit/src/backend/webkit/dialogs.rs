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
use std::time::Instant;

use block2::RcBlock;
use objc2_foundation::NSTimer;

use crate::engine::{DialogAnswer, PopupFeed, DIALOG_TIMEOUT};

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
        // SAFETY: the timer passes itself; it is valid for the call.
        unsafe { timer.as_ref() }.invalidate();
        finish(answer);
    });
    // SAFETY: scheduled on the current (main) run loop and fired there
    // only, so the block's main-thread-only captures never cross threads.
    let _ =
        unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(POLL_SECS, true, &block) };
}
