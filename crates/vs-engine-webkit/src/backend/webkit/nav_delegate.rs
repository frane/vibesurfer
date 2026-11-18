//! `VsNavDelegate` — a custom main-thread-only Obj-C class that
//! implements `WKNavigationDelegate`. Drops the navigation result into
//! a shared slot when the page finishes loading or fails.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol, NSRect, NSURLRequest};
use objc2_web_kit::{
    WKNavigation, WKNavigationAction, WKNavigationDelegate, WKUIDelegate, WKWebView,
    WKWebViewConfiguration, WKWindowFeatures,
};

/// Slot shared between the delegate and the Rust caller. Both ends
/// live on the main thread, so `Rc<RefCell<...>>` is fine.
///
/// `started` / `committed` are recorded separately from `done` because
/// the web-process launch and the page load are different waits. A
/// cold `WKWebView` spends most of its first navigation spawning the
/// web-content process; folding that into the 15s load budget makes
/// `open` return `TIMEOUT` for a page that has not even started.
#[derive(Debug, Default)]
pub(super) struct NavState {
    pub started: bool,
    pub committed: bool,
    pub done: Option<Result<(), String>>,
}

impl NavState {
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }
}

pub(super) type NavSlot = Rc<RefCell<NavState>>;

pub(super) struct NavDelegateIvars {
    pub(super) slot: NavSlot,
    /// A new-window navigation WebKit asked for and we have not
    /// loaded yet. See `create_web_view`.
    pub(super) new_window: RefCell<Option<Retained<NSURLRequest>>>,
}

define_class!(
    /// `WKNavigationDelegate` that signals page-load completion.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "VsNavDelegate"]
    #[ivars = NavDelegateIvars]
    pub(super) struct NavDelegate;

    impl NavDelegate {
        #[unsafe(method(webView:didStartProvisionalNavigation:))]
        fn did_start(&self, _web_view: &WKWebView, _nav: Option<&WKNavigation>) {
            self.ivars().slot.borrow_mut().started = true;
        }

        #[unsafe(method(webView:didCommitNavigation:))]
        fn did_commit(&self, _web_view: &WKWebView, _nav: Option<&WKNavigation>) {
            let mut slot = self.ivars().slot.borrow_mut();
            slot.started = true;
            slot.committed = true;
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, _web_view: &WKWebView, _nav: Option<&WKNavigation>) {
            let mut slot = self.ivars().slot.borrow_mut();
            slot.started = true;
            slot.committed = true;
            slot.done = Some(Ok(()));
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(
            &self,
            _web_view: &WKWebView,
            _nav: Option<&WKNavigation>,
            error: &NSError,
        ) {
            let msg = error.localizedDescription().to_string();
            self.ivars().slot.borrow_mut().done = Some(Err(msg));
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(
            &self,
            _web_view: &WKWebView,
            _nav: Option<&WKNavigation>,
            error: &NSError,
        ) {
            let msg = error.localizedDescription().to_string();
            self.ivars().slot.borrow_mut().done = Some(Err(msg));
        }

        /// Answer WebKit's request for the host window's frame.
        ///
        /// `window.outerWidth` / `outerHeight` / `screenX` / `screenY`
        /// are sourced from the UI client, not from the NSWindow. With
        /// no UI delegate installed WebKit had nothing to ask and the
        /// page saw a zero-sized outer window — which no real browser
        /// reports, and which gives nonsense to any responsive code
        /// deriving browser-chrome height from
        /// `outerHeight - innerHeight`.
        #[unsafe(method(_webView:getWindowFrameWithCompletionHandler:))]
        fn get_window_frame(
            &self,
            web_view: &WKWebView,
            handler: &block2::DynBlock<dyn Fn(NSRect)>,
        ) {
            let frame = web_view
                .window()
                .map_or_else(|| web_view.frame(), |w| w.frame());
            handler.call((frame,));
        }

        /// A `target="_blank"` link or form, or `window.open()`.
        ///
        /// A headless page has nowhere to put a second window, and
        /// without this method WebKit drops the navigation: the click
        /// does nothing and the agent sees an unchanged tree. Park the
        /// request instead; the input primitive that caused it loads it
        /// in this page and waits for it (see `follow_new_window`). A
        /// load started from inside this callback is dropped, because
        /// WebKit is still deciding the originating navigation. The
        /// request carries method and body, so a POSTed form (SAML and
        /// institution-login handoffs) arrives intact.
        #[unsafe(method(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        fn create_web_view(
            &self,
            _web_view: &WKWebView,
            _config: &WKWebViewConfiguration,
            action: &WKNavigationAction,
            _features: &WKWindowFeatures,
        ) -> *mut WKWebView {
            let request = unsafe { action.request() };
            *self.ivars().new_window.borrow_mut() = Some(request);
            std::ptr::null_mut()
        }
    }

    unsafe impl NSObjectProtocol for NavDelegate {}
    unsafe impl WKNavigationDelegate for NavDelegate {}
    unsafe impl WKUIDelegate for NavDelegate {}
);

impl NavDelegate {
    pub(super) fn new(mtm: MainThreadMarker, slot: NavSlot) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(NavDelegateIvars {
            slot,
            new_window: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Shared navigation-result slot. Reused by `navigate` to await an
    /// in-place navigation on the same delegate.
    pub(super) fn slot(&self) -> NavSlot {
        self.ivars().slot.clone()
    }

    /// Whether the page has asked for a new window we have not loaded.
    pub(super) fn has_new_window(&self) -> bool {
        self.ivars().new_window.borrow().is_some()
    }

    /// Take the parked new-window request, if the page asked for one.
    pub(super) fn take_new_window(&self) -> Option<Retained<NSURLRequest>> {
        self.ivars().new_window.borrow_mut().take()
    }
}

/// How long a web-content process may take to accept the first load.
///
/// This is not the page-load budget. A cold `WKWebView` (first open
/// after the daemon starts, or the first open after the process pool
/// has been starved) spends that time in XPC before `didStart` fires.
/// Charging it against the 15s load budget made `open` return
/// `! TIMEOUT 15000ms open` for a navigation that had not begun. 45s
/// is the allowance the integration harness already had to grant the
/// whole open for the same reason.
const LAUNCH_BUDGET: Duration = Duration::from_secs(45);

/// Wait until `slot` finishes, fails, or the load budget expires.
///
/// The load budget starts at provisional navigation, not at
/// `loadRequest`. If the document has committed and subresources then
/// hang, the page is usable and this returns `Ok` — a tracker that
/// never completes is not a failed open.
pub(super) fn wait_for_navigation(
    slot: &NavSlot,
    load_budget: Duration,
    primitive: &'static str,
) -> crate::engine::EngineResult<()> {
    use crate::engine::EngineError;

    use super::eval::run_loop_until;

    let launched = run_loop_until(
        || {
            let state = slot.borrow();
            state.started || state.committed || state.done.is_some()
        },
        LAUNCH_BUDGET,
    );
    if !launched {
        return Err(EngineError::Timeout {
            budget: LAUNCH_BUDGET,
            primitive,
            detail: "web process did not start",
        });
    }
    if let Some(result) = slot.borrow().done.clone() {
        return result.map_err(|msg| EngineError::Other(format!("navigation failed: {msg}")));
    }

    let _finished = run_loop_until(|| slot.borrow().done.is_some(), load_budget);
    let state = slot.borrow();
    match &state.done {
        Some(Ok(())) => Ok(()),
        Some(Err(msg)) => Err(EngineError::Other(format!("navigation failed: {msg}"))),
        // The main document is in. A subresource that never completes
        // (a tracker, an open websocket counted as a load) must not
        // fail the open — the page is readable.
        None if state.committed => Ok(()),
        None => Err(EngineError::Timeout {
            budget: load_budget,
            primitive,
            detail: "navigation did not finish",
        }),
    }
}
