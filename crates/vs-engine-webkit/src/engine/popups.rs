//! Popups and JavaScript dialogs: what a page opened or asked, queued
//! for the daemon, and the answers it sends back.

use std::time::Duration;

#[cfg(doc)]
use super::Engine;
use super::PageHandle;

/// A page another page opened (see [`Engine::set_popups`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Popup {
    /// `None` when the opener was not an engine page (it closed).
    pub opener: Option<PageHandle>,
    pub page: PageHandle,
    /// The URL it was asked to load (empty for `window.open()` with none).
    pub url: String,
}

/// One entry of a [`PopupFeed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopupEvent {
    Opened(Popup),
    /// The page called `window.close()`; the engine releases it.
    Closed(PageHandle),
    /// The page's web content process died (a crash or the OS killing
    /// it). The page stays; its next load starts a new process.
    Crashed(PageHandle),
    /// The page opened `alert()`, `confirm()` or `prompt()` and waits
    /// for [`PopupFeed::answer`] (dismissed after [`DIALOG_TIMEOUT`]).
    Dialog(Dialog),
}

/// A JavaScript dialog a page is blocked on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog {
    pub page: PageHandle,
    pub id: u64,
    /// `alert`, `confirm` or `prompt`.
    pub kind: &'static str,
    pub message: String,
    /// A prompt's default text.
    pub default: String,
}

/// How a dialog is closed: OK (with a prompt's text) or Cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogAnswer {
    pub accept: bool,
    pub text: Option<String>,
}

/// A dialog nobody answers is dismissed after this long, so a page is
/// never left blocked.
pub const DIALOG_TIMEOUT: Duration = Duration::from_secs(10);

/// See [`Engine::popup_feed`]. Carries dialogs too: they need the same
/// off-thread path, since the page's script is blocked until answered.
#[derive(Debug, Default)]
pub struct PopupFeed {
    events: std::sync::Mutex<Vec<PopupEvent>>,
    raised: std::sync::atomic::AtomicU64,
    /// Open dialogs, each with its answer once given. One lock, so an
    /// answer either lands before its dialog closes or is refused.
    open: std::sync::Mutex<std::collections::HashMap<u64, Option<DialogAnswer>>>,
    last_dialog: std::sync::atomic::AtomicU64,
}

impl PopupFeed {
    /// A fresh dialog id, open until [`Self::settle`] closes it.
    pub fn next_dialog_id(&self) -> u64 {
        let id = self
            .last_dialog
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.open.lock().expect("poisoned").insert(id, None);
        id
    }

    /// Answer dialog `id`, from any thread; the engine applies it on its
    /// next check (within a few milliseconds). False when `id` is not
    /// open: unknown, already answered, or dismissed for timing out.
    pub fn answer(&self, id: u64, answer: DialogAnswer) -> bool {
        match self.open.lock().expect("poisoned").get_mut(&id) {
            Some(slot @ None) => {
                *slot = Some(answer);
                true
            }
            _ => false,
        }
    }

    /// The engine's check on dialog `id`. `None` while it waits; once it
    /// has an answer, or `expired` with none (then `Some(None)`: dismiss),
    /// the dialog closes and later answers are refused.
    pub fn settle(&self, id: u64, expired: bool) -> Option<Option<DialogAnswer>> {
        let mut open = self.open.lock().expect("poisoned");
        match open.get(&id) {
            Some(Some(_)) => open.remove(&id),
            Some(None) if expired => open.remove(&id),
            Some(None) => None,
            None => Some(None),
        }
    }

    pub fn push(&self, e: PopupEvent) {
        self.events.lock().expect("poisoned").push(e);
        self.raised
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Events since the last drain, oldest first.
    pub fn drain(&self) -> Vec<PopupEvent> {
        std::mem::take(&mut *self.events.lock().expect("poisoned"))
    }

    /// Events pushed so far.
    pub fn raised(&self) -> u64 {
        self.raised.load(std::sync::atomic::Ordering::SeqCst)
    }
}
