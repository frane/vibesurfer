//! Real popups and JavaScript dialogs on WebKitGTK, for pages whose
//! session asked for them (`Engine::set_popups`). The same model as the
//! macOS backend: a window a page opens becomes a page of its own,
//! reported on the thread-safe [`PopupFeed`] with its handle, and adopted
//! into the backend on the next page lookup; `window.close()` reports and
//! releases the page; `alert`/`confirm`/`prompt` are reported and held
//! until a client answers through the feed, or dismissed after
//! [`DIALOG_TIMEOUT`].
//!
//! The dialog answer is applied by a GLib timer, not a call into the
//! engine: the page's script is blocked while the dialog is up, and the
//! input primitive that opened it is often still waiting on that script.
//! The timer fires inside that wait's main-loop pumping.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use webkit6::{ScriptDialog, ScriptDialogType};

use crate::engine::{Dialog, PageHandle, PopupEvent, PopupFeed, DIALOG_TIMEOUT};

/// A page another page opened, not yet adopted by the backend.
pub(super) struct Born<P> {
    pub handle: PageHandle,
    pub page: P,
}

/// Popups and script-closed pages waiting for the backend, and the
/// page-handle counter (a popup gets its handle at birth, so the daemon
/// can name it before the backend adopts it).
pub(super) struct PopupState<P> {
    pub born: Vec<Born<P>>,
    pub closed: Vec<PageHandle>,
    pub feed: Arc<PopupFeed>,
    last_handle: u64,
    /// The feed count when the current input primitive started.
    pub raised_at_input: u64,
}

impl<P> Default for PopupState<P> {
    fn default() -> Self {
        Self {
            born: Vec::new(),
            closed: Vec::new(),
            feed: Arc::default(),
            last_handle: 0,
            raised_at_input: 0,
        }
    }
}

impl<P> PopupState<P> {
    pub fn alloc(&mut self) -> PageHandle {
        self.last_handle += 1;
        PageHandle(self.last_handle)
    }
}

pub(super) type Popups<P> = Rc<RefCell<PopupState<P>>>;

/// How often a pending dialog checks for its answer.
const POLL: Duration = Duration::from_millis(10);

/// Report `dialog` from page `page` on `feed` and answer it when a
/// client does (or dismiss it after `DIALOG_TIMEOUT`). Returns `true`:
/// the dialog is handled here, WebKit shows nothing.
pub(super) fn hold_dialog(feed: &Arc<PopupFeed>, page: PageHandle, dialog: &ScriptDialog) -> bool {
    let ty = dialog.dialog_type();
    let kind = match ty {
        ScriptDialogType::Confirm | ScriptDialogType::BeforeUnloadConfirm => "confirm",
        ScriptDialogType::Prompt => "prompt",
        _ => "alert",
    };
    let message = dialog.message().map(|m| m.to_string()).unwrap_or_default();
    let default = if ty == ScriptDialogType::Prompt {
        dialog
            .prompt_get_default_text()
            .map(|t| t.to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let id = feed.next_dialog_id();
    feed.push(PopupEvent::Dialog(Dialog {
        page,
        id,
        kind,
        message,
        default: default.clone(),
    }));
    let dialog = dialog.clone();
    let feed = feed.clone();
    let started = Instant::now();
    glib::timeout_add_local(POLL, move || {
        let answer = feed.take_answer(id);
        if answer.is_none() && started.elapsed() < DIALOG_TIMEOUT {
            return glib::ControlFlow::Continue;
        }
        if let Some(a) = answer {
            match dialog.dialog_type() {
                ScriptDialogType::Confirm | ScriptDialogType::BeforeUnloadConfirm => {
                    dialog.confirm_set_confirmed(a.accept);
                }
                // A prompt left unset returns null, as a dismissed one should.
                ScriptDialogType::Prompt if a.accept => {
                    dialog.prompt_set_text(a.text.as_deref().unwrap_or(&default));
                }
                _ => {}
            }
        }
        dialog.close();
        glib::ControlFlow::Break
    });
    true
}

/// A dialog in a page without a popup feed: dismissed at once, as on
/// macOS, so the page is never left blocked on a dialog nobody sees.
pub(super) fn dismiss_dialog(dialog: &ScriptDialog) -> bool {
    if matches!(
        dialog.dialog_type(),
        ScriptDialogType::Confirm | ScriptDialogType::BeforeUnloadConfirm
    ) {
        dialog.confirm_set_confirmed(false);
    }
    dialog.close();
    true
}
