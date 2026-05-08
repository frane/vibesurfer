//! `VsNavDelegate` — a custom main-thread-only Obj-C class that
//! implements `WKNavigationDelegate`. Drops the navigation result into
//! a shared slot when the page finishes loading or fails.

use std::cell::RefCell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol};
use objc2_web_kit::{WKNavigation, WKNavigationDelegate, WKWebView};

/// Slot shared between the delegate and the Rust caller. Both ends
/// live on the main thread, so `Rc<RefCell<...>>` is fine.
pub(super) type NavSlot = Rc<RefCell<Option<Result<(), String>>>>;

pub(super) struct NavDelegateIvars {
    pub(super) slot: NavSlot,
}

define_class!(
    /// `WKNavigationDelegate` that signals page-load completion.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "VsNavDelegate"]
    #[ivars = NavDelegateIvars]
    pub(super) struct NavDelegate;

    impl NavDelegate {
        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, _web_view: &WKWebView, _nav: Option<&WKNavigation>) {
            *self.ivars().slot.borrow_mut() = Some(Ok(()));
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(
            &self,
            _web_view: &WKWebView,
            _nav: Option<&WKNavigation>,
            error: &NSError,
        ) {
            let msg = error.localizedDescription().to_string();
            *self.ivars().slot.borrow_mut() = Some(Err(msg));
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(
            &self,
            _web_view: &WKWebView,
            _nav: Option<&WKNavigation>,
            error: &NSError,
        ) {
            let msg = error.localizedDescription().to_string();
            *self.ivars().slot.borrow_mut() = Some(Err(msg));
        }
    }

    unsafe impl NSObjectProtocol for NavDelegate {}
    unsafe impl WKNavigationDelegate for NavDelegate {}
);

impl NavDelegate {
    pub(super) fn new(mtm: MainThreadMarker, slot: NavSlot) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(NavDelegateIvars { slot });
        unsafe { msg_send![super(this), init] }
    }
}
