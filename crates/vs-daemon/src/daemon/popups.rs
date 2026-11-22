//! Real popup windows for `--popups` sessions.
//!
//! The engine opens a new page for a `target="_blank"` link or
//! `window.open()` on its own, from inside the page's event handling,
//! and reports it on its popup feed. The daemon adopts each one into
//! the opener's session as an ordinary page, and drops pages that
//! closed themselves (`window.close()`). An intercepting session also
//! gets an event line on its `vs_net_poll` stream:
//! `page <opener> <page> <url>` and `close <page>`.
//!
//! The feed is read off the engine thread on purpose: that thread is
//! often still busy with the click that opened the popup, and the
//! popup's first request must not wait for it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use vs_engine_webkit::{Dialog, DialogAnswer, PageHandle, Popup, PopupEvent, PopupFeed};

use super::Daemon;
use crate::page_state::PageState;

/// How often the adoption thread checks the feed. A check is one
/// atomic load.
const TICK: Duration = Duration::from_millis(5);

/// The engine's popup feed, and how many of its events the daemon has
/// handled.
#[derive(Default)]
pub(crate) struct Counters {
    /// Filled by the adoption thread on its first tick, the only place
    /// that asks the engine for it; callers holding the session lock
    /// must not wait on the engine thread.
    feed: Arc<OnceLock<Option<Arc<PopupFeed>>>>,
    handled: Arc<AtomicU64>,
    /// Serializes draining, so events are handled in order.
    drain: Mutex<()>,
}

impl Daemon {
    fn feed(&self) -> Option<Arc<PopupFeed>> {
        self.inner
            .popups
            .feed
            .get_or_init(|| self.inner.engine.popup_feed().ok().flatten())
            .clone()
    }

    /// True once every popup event the engine raised is handled. The
    /// intercept proxy holds a main-frame navigation on it briefly, so
    /// a popup's `page` line reaches the client before its request.
    /// Never calls the engine: before the feed is known, it is settled.
    pub(crate) fn popups_settled_fn(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let feed = self.inner.popups.feed.clone();
        let handled = self.inner.popups.handled.clone();
        Arc::new(move || match feed.get() {
            Some(Some(f)) => handled.load(Ordering::SeqCst) >= f.raised(),
            _ => true,
        })
    }

    /// Turn real popups on for a fresh engine page of a `--popups`
    /// session. Failure leaves the page loading new windows in place.
    pub(crate) fn apply_popups(&self, session_id: &str, handle: PageHandle) {
        let on = self
            .inner
            .sessions
            .lock()
            .expect("poisoned")
            .get(session_id)
            .is_some_and(|s| s.popups);
        if on {
            let _ = self.inner.engine.set_popups(handle, true);
        }
    }

    /// Handle the popup events raised since the last call.
    pub(crate) fn adopt_popups(&self) {
        let Some(feed) = self.feed() else { return };
        if self.inner.popups.handled.load(Ordering::SeqCst) >= feed.raised() {
            return;
        }
        let _order = self.inner.popups.drain.lock().expect("poisoned");
        for e in feed.drain() {
            match e {
                PopupEvent::Opened(p) => self.adopt(&p),
                PopupEvent::Closed(h) => self.forget_closed(h),
                PopupEvent::Dialog(d) => self.report_dialog(&feed, &d),
            }
            self.inner.popups.handled.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn adopt(&self, p: &Popup) {
        let page_id = format!("p_{}", super::short_id());
        let found = p.opener.and_then(|opener| {
            let sessions = self.inner.sessions.lock().expect("poisoned");
            sessions.iter().find_map(|(sid, s)| {
                s.pages
                    .values()
                    .find(|pg| pg.engine_handle == Some(opener))
                    .map(|pg| (sid.clone(), pg.id.clone(), pg.init_scripts.clone()))
            })
        });
        let Some((session_id, opener_id, scripts)) = found else {
            // No session owns the opener any more: nobody can use it.
            let _ = self.inner.engine.close(p.page);
            return;
        };
        let created = self
            .inner
            .store
            .lock()
            .expect("poisoned")
            .create_page(&page_id, &session_id, &p.url)
            .is_ok();
        let net = {
            let mut sessions = self.inner.sessions.lock().expect("poisoned");
            match sessions.get_mut(&session_id) {
                Some(s) if created => {
                    let mut page = PageState::new(page_id.clone(), p.url.clone(), p.page);
                    page.init_scripts = scripts;
                    s.pages.insert(page_id.clone(), page);
                    Some(s.net.clone())
                }
                _ => None,
            }
        };
        match net {
            Some(Some(net)) => net.push_line(format!("page {opener_id} {page_id} {}\n", p.url)),
            Some(None) => {}
            None => {
                let _ = self.inner.engine.close(p.page);
            }
        }
    }

    /// `dialog <page> <id> <kind> <message-b64> <default-b64>` on the
    /// page's event stream; with no stream to answer on, dismissed now.
    fn report_dialog(&self, feed: &PopupFeed, d: &Dialog) {
        use base64::Engine as _;
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let found = {
            let sessions = self.inner.sessions.lock().expect("poisoned");
            sessions.values().find_map(|s| {
                let page = s
                    .pages
                    .values()
                    .find(|pg| pg.engine_handle == Some(d.page))?;
                Some((page.id.clone(), s.net.clone()))
            })
        };
        match found {
            Some((page_id, Some(net))) => net.push_line(format!(
                "dialog {page_id} {} {} {} {}\n",
                d.id,
                d.kind,
                b64(&d.message),
                b64(&d.default)
            )),
            _ => feed.answer(
                d.id,
                DialogAnswer {
                    accept: false,
                    text: None,
                },
            ),
        }
    }

    /// Answer dialog `id` (see `report_dialog`). False when the engine
    /// has no dialogs to answer.
    pub(crate) fn answer_dialog(&self, id: u64, accept: bool, text: Option<String>) -> bool {
        let Some(feed) = self.feed() else {
            return false;
        };
        feed.answer(id, DialogAnswer { accept, text });
        true
    }

    fn forget_closed(&self, handle: PageHandle) {
        let gone = {
            let mut sessions = self.inner.sessions.lock().expect("poisoned");
            sessions.values_mut().find_map(|s| {
                let id = s
                    .pages
                    .values()
                    .find(|pg| pg.engine_handle == Some(handle))
                    .map(|pg| pg.id.clone())?;
                s.pages.remove(&id);
                Some((id, s.net.clone()))
            })
        };
        let Some((page_id, net)) = gone else { return };
        let _ = self
            .inner
            .store
            .lock()
            .expect("poisoned")
            .close_page(&page_id);
        if let Some(net) = net {
            net.push_line(format!("close {page_id}\n"));
        }
    }
}

/// Start the adoption thread. Weak, like the reaper, so it never keeps
/// the daemon alive.
pub(crate) fn spawn(daemon: &Daemon) {
    let weak = std::sync::Arc::downgrade(&daemon.inner);
    let _ = std::thread::Builder::new()
        .name("vs-popups".into())
        .spawn(move || loop {
            std::thread::sleep(TICK);
            let Some(inner) = weak.upgrade() else { break };
            Daemon { inner }.adopt_popups();
        });
}
