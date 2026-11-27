//! Navigating a web view and waiting for the load, within the
//! navigation budget every backend uses.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc;

use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2;
use webview2_com::{ContentLoadingEventHandler, NavigationCompletedEventHandler};
use windows::core::{HSTRING, PCWSTR};

use crate::engine::{EngineError, EngineResult};

use super::wait::pump_until;

/// Navigate `web_view` to `url` and wait for `NavigationCompleted`.
///
/// As on macOS, a page whose document has started loading is usable:
/// if the budget runs out after that (a subresource that never
/// finishes), this returns `Ok`. Before it, the navigation times out.
pub(super) fn navigate_and_wait(
    web_view: &ICoreWebView2,
    url: &str,
    primitive: &'static str,
) -> EngineResult<()> {
    let (tx, rx) = mpsc::channel();
    let completed = NavigationCompletedEventHandler::create(Box::new(move |_, _| {
        let _ = tx.send(());
        Ok(())
    }));
    let committed = Rc::new(Cell::new(false));
    let loading = {
        let committed = committed.clone();
        ContentLoadingEventHandler::create(Box::new(move |_, _| {
            committed.set(true);
            Ok(())
        }))
    };
    let (mut completed_token, mut loading_token) = (0i64, 0i64);
    unsafe { web_view.add_NavigationCompleted(&completed, &raw mut completed_token) }
        .map_err(|e| EngineError::Other(format!("add_NavigationCompleted: {e}")))?;
    if let Err(e) = unsafe { web_view.add_ContentLoading(&loading, &raw mut loading_token) } {
        let _ = unsafe { web_view.remove_NavigationCompleted(completed_token) };
        return Err(EngineError::Other(format!("add_ContentLoading: {e}")));
    }

    let url = HSTRING::from(url);
    let budget = crate::backend::common::nav_budget();
    let waited = unsafe { web_view.Navigate(PCWSTR(url.as_ptr())) }
        .map_err(|e| EngineError::Other(format!("Navigate: {e}")))
        .and_then(
            |()| match pump_until(&rx, budget, primitive, "navigation did not finish") {
                Err(EngineError::Timeout { .. }) if committed.get() => Ok(()),
                other => other,
            },
        );

    // Unsubscribe whatever happened, so a failed navigation does not
    // leave handlers on the page for every later one.
    unsafe {
        let _ = web_view.remove_ContentLoading(loading_token);
        web_view.remove_NavigationCompleted(completed_token)
    }
    .map_err(|e| EngineError::Other(format!("remove_NavigationCompleted: {e}")))?;
    waited
}
