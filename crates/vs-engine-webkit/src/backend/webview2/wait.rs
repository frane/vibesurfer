//! Bounded waits on WebView2's asynchronous calls.
//!
//! Every WebView2 call that does work (creating the browser process or a
//! controller, running script, a DevTools call, reading cookies, a
//! capture) reports through a completion handler, delivered as a window
//! message. `webview2_com`'s own helpers wait for it by pumping messages
//! with no deadline, so one completion that never came wedged the
//! daemon's main thread and every call queued behind it. These waits
//! pump until the handler reports or the budget runs out; a completion
//! that arrives after that is dropped.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW, TranslateMessage, MSG,
    MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT,
};

use crate::engine::{EngineError, EngineResult};

/// Creating a browser process or a controller. A cold start spends
/// seconds before the first completion, as a cold WKWebView does (see
/// the macOS launch budget).
pub(in crate::backend) const SETUP_BUDGET: Duration = Duration::from_secs(45);

/// Any other call: a script, a DevTools call, cookies, a capture.
pub(in crate::backend) const CALL_BUDGET: Duration = Duration::from_secs(30);

/// Pump window messages until `rx` yields a value or `budget` runs out.
pub(in crate::backend) fn pump_until<T>(
    rx: &Receiver<T>,
    budget: Duration,
    primitive: &'static str,
    detail: &'static str,
) -> EngineResult<T> {
    let deadline = Instant::now() + budget;
    let mut msg = MSG::default();
    loop {
        match rx.try_recv() {
            Ok(v) => return Ok(v),
            Err(TryRecvError::Disconnected) => {
                return Err(EngineError::Other(format!(
                    "{primitive}: the completion handler was released without reporting"
                )))
            }
            Err(TryRecvError::Empty) => {}
        }
        // SAFETY: standard message pump on the thread that owns the
        // web views.
        let drained = unsafe {
            let mut any = false;
            while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
                any = true;
            }
            any
        };
        if drained {
            continue;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(EngineError::Timeout {
                budget,
                primitive,
                detail,
            });
        }
        let ms = u32::try_from(left.as_millis()).unwrap_or(u32::MAX).max(1);
        // SAFETY: waits for a message or the timeout; no handles.
        unsafe {
            MsgWaitForMultipleObjectsEx(None, ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
    }
}

/// Start an asynchronous WebView2 call and wait for its completion.
///
/// `handler` builds the completion handler around the sender it is
/// given; `start` makes the call with it. The handler should send even
/// on error, so the failure comes back instead of the timeout.
pub(in crate::backend) fn run_async<H, T>(
    primitive: &'static str,
    budget: Duration,
    handler: impl FnOnce(Sender<EngineResult<T>>) -> H,
    start: impl FnOnce(&H) -> windows::core::Result<()>,
) -> EngineResult<T> {
    let (tx, rx) = mpsc::channel();
    let h = handler(tx);
    start(&h).map_err(|e| EngineError::Other(format!("{primitive}: {e}")))?;
    pump_until(&rx, budget, primitive, "no completion")?
}

/// The error a completion handler reports, named after its call.
pub(in crate::backend) fn failed(primitive: &str, e: &windows::core::Error) -> EngineError {
    EngineError::Other(format!("{primitive}: {e}"))
}
