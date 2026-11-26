//! WebView2 parts of popups, dialogs and per-frame evaluation, kept out
//! of `webview2.rs`'s page bring-up. The model matches the WebKit
//! backends: pages with real popups (`Engine::set_popups`) report new
//! windows, `window.close()` and JavaScript dialogs on the thread-safe
//! [`PopupFeed`]; a dialog is held (a deferral) until a client answers
//! through the feed, or dismissed after [`DIALOG_TIMEOUT`].
//!
//! The dialog answer is applied by a Win32 timer, not a call into the
//! engine: the page's script is blocked while the dialog is up, and the
//! input primitive that opened it is often still waiting on that script.
//! The timer fires inside that wait's message pumping.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2Deferral, ICoreWebView2Frame, ICoreWebView2Frame2,
    ICoreWebView2ScriptDialogOpeningEventArgs, COREWEBVIEW2_SCRIPT_DIALOG_KIND,
    COREWEBVIEW2_SCRIPT_DIALOG_KIND_BEFOREUNLOAD, COREWEBVIEW2_SCRIPT_DIALOG_KIND_CONFIRM,
    COREWEBVIEW2_SCRIPT_DIALOG_KIND_PROMPT,
};
use webview2_com::{
    pwstr_from_str, take_pwstr, ExecuteScriptCompletedHandler, FrameDestroyedEventHandler,
    FrameNavigationStartingEventHandler,
};
use windows::core::{Interface, PWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};

use crate::engine::{
    Dialog, EngineError, EngineResult, PageHandle, PopupEvent, PopupFeed, DIALOG_TIMEOUT,
};

/// A page another page opened, not yet adopted by the backend.
pub(super) struct Born<P> {
    pub handle: PageHandle,
    pub page: P,
}

/// Popups and script-closed pages waiting for the backend, the popup
/// feed, and the page-handle counter (a popup gets its handle at birth,
/// so the daemon can name it before the backend adopts it).
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

// ---------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------

struct PendingDialog {
    feed: Arc<PopupFeed>,
    id: u64,
    args: ICoreWebView2ScriptDialogOpeningEventArgs,
    deferral: ICoreWebView2Deferral,
    kind: COREWEBVIEW2_SCRIPT_DIALOG_KIND,
    default: String,
    started: Instant,
}

thread_local! {
    static DIALOGS: RefCell<Vec<PendingDialog>> = const { RefCell::new(Vec::new()) };
    static TIMER: Cell<usize> = const { Cell::new(0) };
}

/// How often pending dialogs check for their answer, in ms.
const POLL_MS: u32 = 10;

fn read(f: impl FnOnce(*mut PWSTR) -> windows::core::Result<()>) -> String {
    let mut p = PWSTR(std::ptr::null_mut());
    if f(&raw mut p).is_ok() && !p.is_null() {
        take_pwstr(p)
    } else {
        String::new()
    }
}

/// Report the dialog in `args` from page `page` on `feed` and hold it
/// for an answer. Without this the dialog is dismissed when the event
/// handler returns (default dialogs are off).
pub(super) fn hold_dialog(
    feed: &Arc<PopupFeed>,
    page: PageHandle,
    args: &ICoreWebView2ScriptDialogOpeningEventArgs,
) -> windows::core::Result<()> {
    let mut kind = COREWEBVIEW2_SCRIPT_DIALOG_KIND::default();
    unsafe { args.Kind(&raw mut kind) }?;
    let label = if kind == COREWEBVIEW2_SCRIPT_DIALOG_KIND_PROMPT {
        "prompt"
    } else if kind == COREWEBVIEW2_SCRIPT_DIALOG_KIND_CONFIRM
        || kind == COREWEBVIEW2_SCRIPT_DIALOG_KIND_BEFOREUNLOAD
    {
        "confirm"
    } else {
        "alert"
    };
    let message = read(|p| unsafe { args.Message(p) });
    let default = read(|p| unsafe { args.DefaultText(p) });
    let deferral = unsafe { args.GetDeferral() }?;
    let id = feed.next_dialog_id();
    feed.push(PopupEvent::Dialog(Dialog {
        page,
        id,
        kind: label,
        message,
        default: default.clone(),
    }));
    DIALOGS.with(|d| {
        d.borrow_mut().push(PendingDialog {
            feed: feed.clone(),
            id,
            args: args.clone(),
            deferral,
            kind,
            default,
            started: Instant::now(),
        });
    });
    TIMER.with(|t| {
        if t.get() == 0 {
            t.set(unsafe { SetTimer(None, 0, POLL_MS, Some(check_dialogs)) });
        }
    });
    Ok(())
}

unsafe extern "system" fn check_dialogs(_: HWND, _: u32, _: usize, _: u32) {
    let ready: Vec<(PendingDialog, Option<crate::engine::DialogAnswer>)> = DIALOGS.with(|d| {
        let mut d = d.borrow_mut();
        let mut out = Vec::new();
        let mut i = 0;
        while i < d.len() {
            let expired = d[i].started.elapsed() >= DIALOG_TIMEOUT;
            if let Some(answer) = d[i].feed.settle(d[i].id, expired) {
                out.push((d.remove(i), answer));
            } else {
                i += 1;
            }
        }
        out
    });
    for (p, answer) in ready {
        if let Some(a) = answer.filter(|a| a.accept) {
            if p.kind == COREWEBVIEW2_SCRIPT_DIALOG_KIND_PROMPT {
                let text = a.text.unwrap_or(p.default);
                let w = pwstr_from_str(&text);
                let _ = unsafe { p.args.SetResultText(windows::core::PCWSTR(w.0)) };
            }
            let _ = unsafe { p.args.Accept() };
        }
        let _ = unsafe { p.deferral.Complete() };
    }
    if DIALOGS.with(|d| d.borrow().is_empty()) {
        TIMER.with(|t| {
            let id = t.replace(0);
            if id != 0 {
                let _ = unsafe { KillTimer(None, id) };
            }
        });
    }
}

// ---------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------

/// One subframe, as WebView2 reported it.
pub(super) struct W2Frame {
    frame: ICoreWebView2Frame2,
    url: Rc<RefCell<String>>,
    alive: Rc<Cell<bool>>,
    born: u64,
}

pub(super) type Frames = Rc<RefCell<(u64, Vec<W2Frame>)>>;

/// Start tracking `frame` (from `FrameCreated`): its URL as it
/// navigates, and when it goes away.
pub(super) fn track_frame(
    frames: &Frames,
    frame: &ICoreWebView2Frame,
) -> windows::core::Result<()> {
    let f2: ICoreWebView2Frame2 = frame.cast()?;
    let url = Rc::new(RefCell::new(String::new()));
    let alive = Rc::new(Cell::new(true));
    {
        let url = url.clone();
        let handler = FrameNavigationStartingEventHandler::create(Box::new(move |_, args| {
            if let Some(args) = args {
                *url.borrow_mut() = read(|p| unsafe { args.Uri(p) });
            }
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe { f2.add_NavigationStarting(&handler, &raw mut token) }?;
    }
    {
        let alive = alive.clone();
        let handler = FrameDestroyedEventHandler::create(Box::new(move |_, _| {
            alive.set(false);
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe { frame.add_Destroyed(&handler, &raw mut token) }?;
    }
    let mut f = frames.borrow_mut();
    f.0 += 1;
    let born = f.0;
    f.1.retain(|x| x.alive.get());
    f.1.push(W2Frame {
        frame: f2,
        url,
        alive,
        born,
    });
    Ok(())
}

/// The live subframe `frame` names: by URL (the newest frame with it),
/// or `#N`, the Nth subframe, newest first.
pub(super) fn find_frame(frames: &Frames, frame: &str) -> Option<ICoreWebView2Frame2> {
    let f = frames.borrow();
    let mut live: Vec<&W2Frame> = f.1.iter().filter(|x| x.alive.get()).collect();
    live.sort_by_key(|x| std::cmp::Reverse(x.born));
    match frame.strip_prefix('#') {
        Some(n) => live.get(n.parse::<usize>().ok()?).map(|x| x.frame.clone()),
        None => live
            .iter()
            .find(|x| *x.url.borrow() == frame)
            .map(|x| x.frame.clone()),
    }
}

/// `ExecuteScript` in one frame; the JSON-encoded result, as for the
/// main frame.
pub(super) fn execute_in_frame(frame: &ICoreWebView2Frame2, js: &str) -> EngineResult<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let js = js.to_string();
    let target = frame.clone();
    ExecuteScriptCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            let w = pwstr_from_str(&js);
            unsafe { target.ExecuteScript(windows::core::PCWSTR(w.0), &handler) }
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |error_code, result_json| {
            error_code?;
            let _ = tx.send(result_json);
            Ok(())
        }),
    )
    .map_err(|e| EngineError::Other(format!("ExecuteScript in frame: {e:?}")))?;
    rx.recv()
        .map_err(|_| EngineError::Other("ExecuteScript in frame: channel closed".into()))
}
