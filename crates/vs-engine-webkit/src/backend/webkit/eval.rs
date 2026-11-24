//! Cocoa main-thread run-loop pump + WKWebView JS-evaluation glue.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use block2::RcBlock;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSError, NSString};
use objc2_web_kit::WKWebView;

use crate::engine::{EngineError, EngineResult};

/// Pump the current thread's run loop in `kCFRunLoopDefaultMode` until
/// `done()` returns `true` or `budget` elapses. Caller must be on the
/// main thread.
pub(super) fn run_loop_until<F: FnMut() -> bool>(mut done: F, budget: Duration) -> bool {
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSRunLoop};
    let deadline = std::time::Instant::now() + budget;
    let run_loop = NSRunLoop::currentRunLoop();
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        if done() {
            return true;
        }
        // Up to 50ms per slice, never past the deadline: runMode only
        // returns early when a source fires, so a full slice turned a
        // 2ms settle into a 50ms one (every click paid ~60ms for it).
        let left = deadline.duration_since(now).as_secs_f64().min(0.05);
        let slice = NSDate::dateWithTimeIntervalSinceNow(left);
        unsafe { run_loop.runMode_beforeDate(NSDefaultRunLoopMode, &slice) };
    }
    done()
}

/// Evaluate `js` on `web_view` and return the result as a String.
/// Pumps the main run loop until the WKWebView completion handler
/// fires or `budget` elapses.
pub(super) fn eval_js_string(
    web_view: &WKWebView,
    js: &str,
    budget: Duration,
) -> EngineResult<String> {
    eval_js_string_in(web_view, None, js, budget)
}

/// [`eval_js_string`] in one frame of the page (its page world), or
/// the main frame for `None`.
pub(super) fn eval_js_string_in(
    web_view: &WKWebView,
    frame: Option<&objc2_web_kit::WKFrameInfo>,
    js: &str,
    budget: Duration,
) -> EngineResult<String> {
    let slot: Rc<RefCell<Option<Result<String, String>>>> = Rc::new(RefCell::new(None));
    let slot_for_block = slot.clone();
    let block = RcBlock::new(move |result: *mut AnyObject, error: *mut NSError| {
        if !error.is_null() {
            let err = unsafe { &*error };
            *slot_for_block.borrow_mut() = Some(Err(err.localizedDescription().to_string()));
            return;
        }
        if result.is_null() {
            *slot_for_block.borrow_mut() = Some(Ok(String::new()));
            return;
        }
        // Callers ask for strings, but a script can return anything:
        // take a non-string by its description rather than crash.
        let obj: &AnyObject = unsafe { &*result };
        let text = if let Some(s) = obj.downcast_ref::<NSString>() {
            s.to_string()
        } else {
            let d: objc2::rc::Retained<NSString> = unsafe { objc2::msg_send![obj, description] };
            d.to_string()
        };
        *slot_for_block.borrow_mut() = Some(Ok(text));
    });
    let ns_js = NSString::from_str(js);
    match frame {
        None => unsafe {
            web_view.evaluateJavaScript_completionHandler(&ns_js, Some(&block));
        },
        Some(frame) => unsafe {
            let world =
                objc2_web_kit::WKContentWorld::pageWorld(objc2::MainThreadOnly::mtm(web_view));
            web_view.evaluateJavaScript_inFrame_inContentWorld_completionHandler(
                &ns_js,
                Some(frame),
                &world,
                Some(&block),
            );
        },
    }
    // The budget leaves out time a dialog is up: the script that opened
    // it cannot finish before it is answered.
    let start = std::time::Instant::now();
    let dialogs_before = super::dialogs::open_time();
    let mut ok = false;
    loop {
        let allowed = budget + super::dialogs::open_time().saturating_sub(dialogs_before);
        let left = allowed.saturating_sub(start.elapsed());
        if left.is_zero() {
            break;
        }
        let slot_check = slot.clone();
        if run_loop_until(
            move || slot_check.borrow().is_some(),
            left.min(Duration::from_millis(200)),
        ) {
            ok = true;
            break;
        }
    }
    if !ok {
        return Err(EngineError::Timeout {
            budget,
            primitive: "eval",
            detail: "",
        });
    }
    let result = slot.borrow_mut().take();
    match result {
        Some(Ok(s)) => Ok(s),
        Some(Err(msg)) => Err(EngineError::Other(format!("eval failed: {msg}"))),
        None => Err(EngineError::Other("eval completed without a result".into())),
    }
}
