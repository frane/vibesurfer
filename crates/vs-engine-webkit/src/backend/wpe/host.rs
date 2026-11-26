//! Building a page's host: the web view in its window with the user
//! agent, inspector bridge, download shim and handlers, and evaluating
//! JavaScript in it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use glib::prelude::*;
use gtk4 as gtk;
use webkit6::prelude::*;
use webkit6::{UserContentInjectedFrames, UserScript, UserScriptInjectionTime, WebView};

use super::input::window_title;
use super::loading::{install_download_capture, install_response_capture};
use super::{run_loop_until, Popups, WpePage};
use crate::engine::{EngineError, EngineResult, PageHandle};

use crate::backend::native_download::SharedDownloads;

/// Everything a web view needs to be a page: a host window on the X
/// display, the user agent, the inspector bridge, download and response
/// capture, and the new-window, close and dialog handlers. Used for
/// pages `open` creates and for windows a page opens (`create`), so a
/// popup is the same kind of page. The caller sets `me` and the window
/// title once the page has a handle.
#[allow(clippy::too_many_lines)]
pub(super) fn host_page(web_view: &WebView, popups: &Popups) -> WpePage {
    // No native context menu: the page still gets its contextmenu
    // event, and no menu stays open to take the next click.
    web_view.connect_context_menu(|_, _, _| true);
    crate::backend::wpe_wheel::install(web_view);
    // Hidden host window: XTest input needs a real surface on the
    // display server to land on (see `open`). It shows at the screen's
    // origin until `place` moves it; no placed page keeps that tile.
    let window = gtk::Window::new();
    window.set_decorated(false);
    window.set_default_size(1280, 800);
    window.set_child(Some(web_view));
    window.present();

    // Pin the User-Agent to a current Safari string so anti-bot
    // fingerprinters don't flag the WebKitGTK default. See the commit
    // log for `crate::engine::DEFAULT_USER_AGENT` rationale.
    if let Some(settings) = WebViewExt::settings(web_view) {
        settings.set_user_agent(Some(crate::engine::DEFAULT_USER_AGENT));
        // Expose `navigator.mediaDevices`. WebKitGTK gates the
        // MediaDevices surface off by default, so the object is simply
        // absent and anything feature-detecting camera or mic support
        // sees a browser that cannot exist.
        settings.set_enable_media_stream(true);
        // `act click` on this engine is a JS click, not a user gesture,
        // so blank-target navigation has to be allowed without one.
        settings.set_javascript_can_open_windows_automatically(true);
    }

    // Inspector capture wiring — install the user script and register
    // both message channels before any load, so the bridge is live by
    // document-start.
    let inspector = crate::backend::inspector_bridge::InspectorSlots::new(
        crate::inspector::DEFAULT_BUFFER_CAPACITY,
    );
    let inspector_installed = install_inspector(web_view, &inspector);
    install_download_shim(web_view);
    let frames = crate::backend::wpe_frames::install(web_view);

    let popups_on = Rc::new(Cell::new(false));
    let me: Rc<Cell<Option<PageHandle>>> = Rc::new(Cell::new(None));

    // A new window (`target="_blank"`, `window.open()`). With popups on
    // it becomes a page of its own, built on a related view (same
    // network session and web process, so `window.opener` works) and
    // reported on the feed. Otherwise there is no second window: park
    // the request, and the input primitive that caused it loads it in
    // this page (`follow_new_window`).
    let new_window: Rc<RefCell<Option<webkit6::URIRequest>>> = Rc::new(RefCell::new(None));
    {
        let parked = new_window.clone();
        let on = popups_on.clone();
        let me = me.clone();
        let popups = popups.clone();
        web_view.connect_create(move |view, action| {
            if !on.get() {
                *parked.borrow_mut() = action.request();
                return None;
            }
            let url = action
                .request()
                .and_then(|r| r.uri())
                .map(|u| u.to_string())
                .unwrap_or_default();
            let child = WebView::builder().related_view(view).build();
            let page = host_page(&child, &popups);
            page.popups_on.set(true);
            let mut s = popups.borrow_mut();
            let handle = s.alloc();
            page.me.set(Some(handle));
            page.window.set_title(Some(&window_title(handle)));
            s.born
                .push(crate::backend::wpe_popups::Born { handle, page });
            s.feed
                .push(crate::engine::PopupEvent::Opened(crate::engine::Popup {
                    opener: me.get(),
                    page: handle,
                    url,
                }));
            Some(child.upcast())
        });
    }
    // `window.close()`: with popups on, report it; the backend releases
    // the page on its next lookup.
    {
        let on = popups_on.clone();
        let me = me.clone();
        let popups = popups.clone();
        web_view.connect_close(move |_| {
            if let (true, Some(h)) = (on.get(), me.get()) {
                let mut s = popups.borrow_mut();
                s.closed.push(h);
                s.feed.push(crate::engine::PopupEvent::Closed(h));
            }
        });
    }
    // The web process died: report it.
    {
        let on = popups_on.clone();
        let me = me.clone();
        let popups = popups.clone();
        web_view.connect_web_process_terminated(move |_, _| {
            if let (true, Some(h)) = (on.get(), me.get()) {
                popups
                    .borrow_mut()
                    .feed
                    .push(crate::engine::PopupEvent::Crashed(h));
            }
        });
    }
    // `alert`/`confirm`/`prompt`: with popups on, report and hold for
    // an answer; otherwise dismiss at once, as on macOS.
    {
        let on = popups_on.clone();
        let me = me.clone();
        let popups = popups.clone();
        web_view.connect_script_dialog(move |_, dialog| match (on.get(), me.get()) {
            (true, Some(h)) => {
                let feed = popups.borrow().feed.clone();
                crate::backend::wpe_popups::hold_dialog(&feed, h, dialog)
            }
            _ => crate::backend::wpe_popups::dismiss_dialog(dialog),
        });
    }

    let downloads = SharedDownloads::default();
    let download_hook = install_download_capture(web_view, &downloads);
    let main_response = install_response_capture(web_view);
    WpePage {
        web_view: web_view.clone(),
        window,
        inspector,
        inspector_installed,
        cookie_baseline: std::cell::RefCell::new(None),
        cookie_next_seq: std::cell::RefCell::new(0),
        last_mouse: Cell::new(vs_humanize::Point { x: 0.0, y: 0.0 }),
        new_window,
        downloads,
        download_hook,
        main_response,
        popups_on,
        me,
        frames,
        origin: Cell::new((0.0, 0.0)),
    }
}

pub(super) fn eval_js_string(
    web_view: &WebView,
    js: &str,
    budget: Duration,
) -> EngineResult<String> {
    let slot: Rc<RefCell<Option<Result<String, String>>>> = Rc::new(RefCell::new(None));
    let slot_for_cb = slot.clone();

    let cancellable = webkit6::gio::Cancellable::new();
    web_view.evaluate_javascript(
        js,
        None,
        None,
        Some(&cancellable),
        move |result| match result {
            Ok(value) => {
                let s = value.to_string();
                *slot_for_cb.borrow_mut() = Some(Ok(s));
            }
            Err(err) => {
                *slot_for_cb.borrow_mut() = Some(Err(err.to_string()));
            }
        },
    );

    let slot_check = slot.clone();
    let ok = run_loop_until(move || slot_check.borrow().is_some(), budget);
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

/// Add the download-capture shim as a document-start script in every
/// frame. WebKitGTK does emit a `download-started` signal, but the
/// shim covers what that signal cannot see — an in-page `blob:` save
/// whose object URL is revoked a tick later — and keeps the capture
/// buffer identical across all three backends.
pub(super) fn install_download_shim(web_view: &WebView) {
    let Some(manager) = web_view.user_content_manager() else {
        return;
    };
    manager.add_script(&UserScript::new(
        crate::backend::common::DOWNLOAD_SHIM_JS,
        UserContentInjectedFrames::AllFrames,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
    // No native drags (see NO_NATIVE_DRAG_JS).
    manager.add_script(&UserScript::new(
        crate::backend::common::NO_NATIVE_DRAG_JS,
        UserContentInjectedFrames::AllFrames,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
}

pub(super) fn install_inspector(
    web_view: &WebView,
    slots: &crate::backend::inspector_bridge::InspectorSlots,
) -> bool {
    use crate::backend::inspector_bridge::{
        self, InspectorSlots, NetworkIngestSlot, CONSOLE_HANDLER, NETWORK_HANDLER,
    };

    if std::env::var_os("VS_DISABLE_INSPECTOR").is_some() {
        return false;
    }

    let manager = web_view.user_content_manager().expect(
        "WebKitGTK WebView is missing a UserContentManager — should not happen on a default WebView",
    );

    let script = UserScript::new(
        inspector_bridge::SCRIPT,
        UserContentInjectedFrames::AllFrames,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    );
    manager.add_script(&script);

    if !manager.register_script_message_handler(CONSOLE_HANDLER, None) {
        return false;
    }
    if !manager.register_script_message_handler(NETWORK_HANDLER, None) {
        return false;
    }

    let console_slots: InspectorSlots = slots.clone();
    manager.connect_script_message_received(Some(CONSOLE_HANDLER), move |_, value| {
        let json = value.to_string();
        let mut buf = console_slots.console.borrow_mut();
        inspector_bridge::ingest_console(&mut buf, &json);
    });
    let network_slots: InspectorSlots = slots.clone();
    manager.connect_script_message_received(Some(NETWORK_HANDLER), move |_, value| {
        let json = value.to_string();
        let mut entries = network_slots.network.borrow_mut();
        let mut details = network_slots.details.borrow_mut();
        let mut pending = network_slots.pending.borrow_mut();
        inspector_bridge::ingest_network(
            NetworkIngestSlot {
                entries: &mut entries,
                details: &mut details,
                pending: &mut pending,
            },
            &json,
        );
    });
    true
}
