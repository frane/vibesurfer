//! `VsHeadlessWindow` — an `NSWindow` subclass that reports itself
//! visible and unoccluded.
//!
//! A headless `WKWebView` lives in a window we never order on screen, so
//! WebKit computes the page's activity state as "not visible" and the
//! web-content process *suspends* DOM timers and `requestAnimationFrame`
//! (not just throttles them). Libraries that defer teardown to those —
//! Radix UI / Floating UI: the RemoveScroll `body{pointer-events:none}`
//! release and popper unmount — then never run, wedging the page to
//! input after a Select/menu commits.
//!
//! WebKit derives that visibility from the host window's `isVisible` /
//! `occlusionState`. This subclass overrides exactly those two queries
//! to report visible, so the page stays "visible" and its timers/rAF
//! keep running — **without ever ordering the window on screen**. Nothing
//! is shown to the user; we only change the answers WebKit reads.

use objc2::rc::Retained;
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSScreen, NSWindow, NSWindowOcclusionState, NSWindowStyleMask,
};
use objc2_foundation::NSRect;

define_class!(
    #[unsafe(super = NSWindow)]
    #[thread_kind = MainThreadOnly]
    #[name = "VsHeadlessWindow"]
    pub(super) struct HeadlessWindow;

    impl HeadlessWindow {
        /// Always report visible. The window is never ordered on screen;
        /// this only changes what WebKit reads when deciding whether to
        /// suspend the page.
        #[unsafe(method(isVisible))]
        fn is_visible(&self) -> bool {
            true
        }

        /// Always report unoccluded/visible for the same reason.
        #[unsafe(method(occlusionState))]
        fn occlusion_state(&self) -> NSWindowOcclusionState {
            NSWindowOcclusionState::Visible
        }

        /// Report the window as key, so the page has focus.
        ///
        /// A window that is never ordered on screen is never key, so
        /// `document.hasFocus()` returned false and the page believed
        /// it was in a background tab. That breaks ordinary things well
        /// beyond bot checks: `autofocus`, `:focus-visible`, IME
        /// composition, and any SPA that defers work until focus.
        /// Same technique and same justification as `isVisible` above —
        /// we change the answer WebKit reads, not what is displayed.
        #[unsafe(method(isKeyWindow))]
        fn is_key_window(&self) -> bool {
            true
        }

        /// A borderless window is not key-eligible by default, which
        /// would make `isKeyWindow` inconsistent with what AppKit
        /// believes.
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        /// Report a real screen for a window that is on none.
        ///
        /// A window never ordered in has no `screen`, and WebKit walks
        /// that to answer `window.outerWidth` / `outerHeight` /
        /// `screenX` / `screenY` — which all came back 0. No browser
        /// reports a zero-sized outer window, and responsive code that
        /// derives browser-chrome height from
        /// `outerHeight - innerHeight` gets nonsense from it.
        #[unsafe(method(screen))]
        fn screen(&self) -> *mut NSScreen {
            // `-screen` is a +0 getter, so hand back an unretained
            // pointer: the main screen is owned by AppKit and outlives
            // this window.
            NSScreen::mainScreen(MainThreadMarker::from(self))
                .map_or(std::ptr::null_mut(), |s| Retained::as_ptr(&s).cast_mut())
        }
    }
);

impl HeadlessWindow {
    /// Construct the offscreen host window (upcast to `NSWindow`). Same
    /// args as `NSWindow::initWithContentRect_styleMask_backing_defer`.
    pub(super) fn host(
        mtm: MainThreadMarker,
        frame: NSRect,
        style: NSWindowStyleMask,
        backing: NSBackingStoreType,
    ) -> Retained<NSWindow> {
        let this: Retained<Self> = unsafe {
            msg_send![
                Self::alloc(mtm),
                initWithContentRect: frame,
                styleMask: style,
                backing: backing,
                defer: false,
            ]
        };
        this.into_super()
    }
}
