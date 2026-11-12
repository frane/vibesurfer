//! `VsHeadlessWebView`: a `WKWebView` that never starts a system drag.
//!
//! A trusted `mouseDown:` followed by `mouseDragged:` over anything
//! WebKit considers draggable (a link, an image, `draggable="true"`,
//! selected text) makes WebKit call
//! `beginDraggingSessionWithItems:event:source:` on the web view. That
//! hands a real drag to the window server: a drag image window appears
//! on the user's screen, the session waits for a hardware mouse-up our
//! synthesized `mouseUp:` never delivers, and the daemon's main thread
//! wedges. Seen on macOS 27 as a hung `vs drag` / `vs click-at` on
//! React Flow nodes and a vs window painted over another app.
//!
//! The override swallows the call, so no session reaches the window
//! server. WebKit still believes a drag is in flight, so the next
//! synthesized mouse-up must end it ([`end_suppressed_drag`]); without
//! that the page stays in drag state and ignores further mouse input.
//! Pages still get their HTML5 drag events from the JS chain `vs drag`
//! dispatches (`build_html5_drag_js`).

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{NSPoint, NSRect};
use objc2_web_kit::{WKWebView, WKWebViewConfiguration};

thread_local! {
    /// Set when WebKit asked for a drag session we refused. All web
    /// views live on the main thread and input is serialized there, so
    /// one flag is enough.
    static DRAG_SUPPRESSED: Cell<bool> = const { Cell::new(false) };
}

define_class!(
    #[unsafe(super = WKWebView)]
    #[thread_kind = MainThreadOnly]
    #[name = "VsHeadlessWebView"]
    pub(super) struct HeadlessWebView;

    impl HeadlessWebView {
        /// Refuse the system drag. Returns nil; WebKit only messages
        /// the session, and messages to nil are no-ops.
        #[unsafe(method(beginDraggingSessionWithItems:event:source:))]
        fn begin_dragging_session(
            &self,
            _items: *mut AnyObject,
            _event: *mut AnyObject,
            _source: *mut AnyObject,
        ) -> *mut AnyObject {
            DRAG_SUPPRESSED.with(|f| f.set(true));
            std::ptr::null_mut()
        }
    }
);

impl HeadlessWebView {
    /// Same as `WKWebView::initWithFrame_configuration`, upcast.
    pub(super) fn create(
        mtm: MainThreadMarker,
        frame: NSRect,
        config: &WKWebViewConfiguration,
    ) -> Retained<WKWebView> {
        let this: Retained<Self> = unsafe {
            msg_send![
                Self::alloc(mtm),
                initWithFrame: frame,
                configuration: config,
            ]
        };
        this.into_super()
    }
}

/// End a drag WebKit started and we suppressed, at window point `at`.
/// Call after every synthesized mouse-up. A no-op when no drag began.
pub(super) fn end_suppressed_drag(web_view: &WKWebView, at: NSPoint) {
    if !DRAG_SUPPRESSED.with(|f| f.replace(false)) {
        return;
    }
    // `draggingSession:endedAtPoint:operation:` is the NSDraggingSource
    // callback WKWebView implements; it sends dragend to the page and
    // clears WebKit's drag state. The point is in screen coordinates;
    // the host window sits at the origin, so window coordinates match.
    // NSDragOperationNone = 0.
    let session: *mut AnyObject = std::ptr::null_mut();
    unsafe {
        let _: () = msg_send![
            web_view,
            draggingSession: session,
            endedAtPoint: at,
            operation: 0usize,
        ];
    }
}
