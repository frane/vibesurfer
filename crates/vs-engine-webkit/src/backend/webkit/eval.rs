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
    while std::time::Instant::now() < deadline {
        if done() {
            return true;
        }
        // 50ms slices: snappy enough for completion, no CPU spin.
        let slice = NSDate::dateWithTimeIntervalSinceNow(0.05);
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
        let s: &NSString = unsafe { &*(result as *const NSString) };
        *slot_for_block.borrow_mut() = Some(Ok(s.to_string()));
    });
    let ns_js = NSString::from_str(js);
    unsafe {
        web_view.evaluateJavaScript_completionHandler(&ns_js, Some(&block));
    }
    let slot_check = slot.clone();
    let ok = run_loop_until(move || slot_check.borrow().is_some(), budget);
    if !ok {
        return Err(EngineError::Timeout {
            budget,
            primitive: "eval",
        });
    }
    let result = slot.borrow_mut().take();
    match result {
        Some(Ok(s)) => Ok(s),
        Some(Err(msg)) => Err(EngineError::Other(format!("eval failed: {msg}"))),
        None => Err(EngineError::Other("eval completed without a result".into())),
    }
}
