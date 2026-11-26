//! Popups: windows a page opens, queued for the backend to adopt as
//! pages, and the feed that tells the daemon about them.

use std::cell::RefCell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::NSWindow;
use objc2_web_kit::{WKWebView, WKWebViewConfiguration};

use super::host_view;
use super::nav_delegate::NavDelegate;
use crate::engine::PageHandle;

/// A window a page opened, waiting for the backend to adopt it.
pub(super) struct PopupBirth {
    pub(super) handle: PageHandle,
    pub(super) opener: Option<PageHandle>,
    pub(super) web_view: Retained<WKWebView>,
    pub(super) window: Retained<NSWindow>,
    pub(super) delegate: Retained<NavDelegate>,
}

/// New windows and script-closed pages, for pages with real popups,
/// and the page-handle counter (a popup gets its handle at birth, so
/// the daemon can name it before the backend adopts it).
#[derive(Default)]
pub(super) struct PopupState {
    pub(super) born: Vec<PopupBirth>,
    pub(super) closed: Vec<PageHandle>,
    pub(super) feed: std::sync::Arc<crate::engine::PopupFeed>,
    pub(super) last_handle: u64,
    /// The feed count when the current input primitive started.
    pub(super) raised_at_input: u64,
}

impl PopupState {
    pub(super) fn alloc(&mut self) -> PageHandle {
        self.last_handle += 1;
        PageHandle(self.last_handle)
    }
}

pub(super) type PopupQueue = Rc<RefCell<PopupState>>;

/// Build the window WebKit asked for from `opener` (UI delegate
/// `createWebView`) at the opener's size, and report it. The
/// configuration is WebKit's: same process pool, data store and user
/// content controller, so the opener's scripts run in it too.
pub(super) fn spawn_popup(
    mtm: MainThreadMarker,
    opener: Option<PageHandle>,
    opener_view: &WKWebView,
    config: &WKWebViewConfiguration,
    url: String,
    ca: Option<Vec<u8>>,
    queue: &PopupQueue,
) -> Retained<WKWebView> {
    let size = opener_view.frame().size;
    let (web_view, window, delegate) = host_view(mtm, config, size, ca);
    let mut q = queue.borrow_mut();
    let handle = q.alloc();
    delegate.set_popups(Some(queue.clone()), Some(handle));
    q.born.push(PopupBirth {
        handle,
        opener,
        web_view: web_view.clone(),
        window,
        delegate,
    });
    q.feed
        .push(crate::engine::PopupEvent::Opened(crate::engine::Popup {
            opener,
            page: handle,
            url,
        }));
    web_view
}

/// `window.close()` from `page`: report it; the backend releases it.
pub(super) fn popup_closed(queue: &PopupQueue, page: PageHandle) {
    let mut q = queue.borrow_mut();
    q.closed.push(page);
    q.feed.push(crate::engine::PopupEvent::Closed(page));
}
