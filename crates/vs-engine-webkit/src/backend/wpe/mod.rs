//! Linux backend — real `WebKitGTK 6` driven via `webkit6` + `glib`.
//!
//! # Threading model
//!
//! WebKitGTK is bound to the GLib main context of the thread that
//! initialized GTK. Methods on [`WpeBackend`] must be called from that
//! thread; the type is `!Send`. Production wiring (follow-up):
//! `vs-cli::serve` on Linux must put GTK on the OS main thread and
//! dispatch engine calls there via `MainContext::invoke`.
//!
//! # Status
//!
//! Implements every primitive on [`Engine`]. The `engine-tests` workflow
//! runs the real-browser cell suite on it under Xvfb, with
//! `libwebkitgtk-6.0-dev` and `libgtk-4-dev` installed. Not built on macOS.

mod cookies;
mod engine_impl;
mod host;
mod input;
mod loading;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

// `gtk4` is a direct dep so we can enable its `v4_10` feature (webkit6
// needs `gtk::Accessible`); alias it to `gtk` to keep the existing refs.
use gtk4 as gtk;
use webkit6::prelude::*;
use webkit6::WebView;

use crate::backend::native_download::SharedDownloads;
use crate::engine::{Engine, EngineError, EngineResult, PageHandle};

use host::eval_js_string;
use input::{park_core_pointer, window_title};
use loading::wait_for_load;

// =============================================================================
// Per-page state
// =============================================================================

struct WpePage {
    web_view: WebView,
    /// Hidden host window that owns `web_view` as its single child.
    /// Required so the WebView has a `GdkSurface` on the X display —
    /// without it XTest's synthetic input has no window to land on.
    /// Created in `open()`, dropped on `close()`.
    window: gtk::Window,
    inspector: super::inspector_bridge::InspectorSlots,
    /// True if the inspector install path actually succeeded for this
    /// page (script + handlers registered without bail). Read by
    /// `Engine::capabilities()` and the daemon `vs_inspect` gate.
    inspector_installed: bool,
    cookie_baseline: std::cell::RefCell<Option<Vec<super::auth::CookieData>>>,
    cookie_next_seq: std::cell::RefCell<u64>,
    /// Last known cursor position in screen-absolute CSS px. Updated
    /// after every `cursor_op` so the next humanized lead-in starts
    /// where the previous one ended. Defaults to (0, 0) on `open`.
    last_mouse: Cell<vs_humanize::Point>,
    /// A new-window navigation (`target="_blank"`, `window.open()`)
    /// the page asked for and we have not loaded yet. Set by the
    /// `create` handler installed in `open`.
    new_window: Rc<RefCell<Option<webkit6::URIRequest>>>,
    /// Downloads this page's responses turned into.
    downloads: SharedDownloads,
    /// The shared network session's `download-started` handler for
    /// this page. Disconnected on close.
    download_hook: Option<(webkit6::NetworkSession, glib::SignalHandlerId)>,
    /// HTTP response of the latest main-frame navigation. Kept by the
    /// `load-changed` handler from `install_response_capture`.
    main_response: Rc<RefCell<Option<crate::inspector::MainResponse>>>,
    /// The page's session asked for real popups (`Engine::set_popups`):
    /// a new window becomes a page, `window.close()` releases it, and
    /// dialogs wait for an answer. Read by the view's signal handlers.
    popups_on: Rc<Cell<bool>>,
    /// The page's own handle, for those handlers (set once allocated).
    me: Rc<Cell<Option<PageHandle>>>,
    /// Subframe agents, for evaluating in one frame (`wpe_frames`).
    frames: Option<super::wpe_frames::FrameAgents>,
    /// Screen position of the host window's top-left corner: its tile
    /// (see `WpeBackend::place`), or the origin.
    origin: Cell<(f64, f64)>,
}

type Popups = super::wpe_popups::Popups<WpePage>;

/// Size of the screen tile each page window gets when the display is
/// large enough for several (see `WpeBackend::place`). Larger than any
/// usual viewport, so a page's window stays inside its tile.
const TILE_W: i32 = 1600;
const TILE_H: i32 = 1000;

// =============================================================================
// Backend
// =============================================================================

/// Real WebKitGTK 6 backend. Construct on the GTK main thread; all
/// subsequent calls must come from the same thread.
pub struct WpeBackend {
    pages: HashMap<PageHandle, WpePage>,
    /// Popups and closed pages from the views' signal handlers, the
    /// popup feed, and the page-handle counter.
    popups: Popups,
    captures_dir: Option<PathBuf>,
    /// Ephemeral network sessions for isolated sessions, by key. One
    /// per key so every page of the session shares cookies and storage.
    isolated_sessions: HashMap<String, webkit6::NetworkSession>,
    /// Which page holds which screen tile (see `place`).
    tiles: HashMap<i32, PageHandle>,
    /// Pages whose first load is still running, by ticket (see
    /// `open_start`), and the next ticket.
    opening: HashMap<u64, Opening>,
    next_ticket: u64,
}

/// A page `open_start` began whose first load has not ended yet.
struct Opening {
    page: WpePage,
    /// Set by the load-finished or load-failed handler.
    slot: Rc<RefCell<Option<Result<(), String>>>>,
    signals: Vec<glib::SignalHandlerId>,
    deadline: std::time::Instant,
    budget: Duration,
}

impl Default for WpeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WpeBackend {
    /// Build a backend. Caller must already have initialized GTK on
    /// this thread (via `gtk4::init()`); production wiring lives in
    /// `vs-cli::serve` on Linux. Constructing on a non-GTK thread will
    /// panic the first time a `WebView` operation runs.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pages: HashMap::new(),
            popups: Popups::default(),
            captures_dir: None,
            isolated_sessions: HashMap::new(),
            tiles: HashMap::new(),
            opening: HashMap::new(),
            next_ticket: 0,
        }
    }

    /// Pin the directory where `capture` writes PNGs. Defaults to a
    /// system temp subdirectory.
    #[must_use]
    pub fn with_capture_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.captures_dir = Some(dir.into());
        self
    }

    fn alloc_handle(&mut self) -> PageHandle {
        self.popups.borrow_mut().alloc()
    }

    /// Take in the windows pages opened and release the pages that
    /// closed themselves since the last call. Runs before any page
    /// lookup, so a popup's handle works as soon as it is reported.
    fn sync_popups(&mut self) {
        let (born, closed) = {
            let mut s = self.popups.borrow_mut();
            (std::mem::take(&mut s.born), std::mem::take(&mut s.closed))
        };
        for b in born {
            self.pages.insert(b.handle, b.page);
            self.place(b.handle);
        }
        for h in closed {
            let _ = self.close(h);
        }
    }

    /// Give a new page's window a screen tile of its own, when the
    /// display has room for several. XTest input goes by screen
    /// position and X has one pointer and one stacking order, so with
    /// windows stacked at one spot, pages took each other's clicks and
    /// hover state; apart, the pointer over one page is over no other.
    /// On a smaller display the windows stay at the origin and input
    /// raises its window first. Then park the pointer off every window,
    /// so the page does not open hovered.
    ///
    /// Tile 0, at the screen's origin, is where new windows show while
    /// their first load runs (the engine serves other pages meanwhile),
    /// so no page keeps it: a page there took clicks meant for it on
    /// the window that opened on top.
    fn place(&mut self, handle: PageHandle) {
        let Ok(input) = super::wpe_input::dispatcher() else {
            return;
        };
        let Some((width, height)) = input.screen_size() else {
            return;
        };
        let (cols, rows) = (width / TILE_W, height / TILE_H);
        let title = window_title(handle);
        let free = (1..cols * rows).find(|i| !self.tiles.contains_key(i));
        // Where to park the page's pointer: past its window inside its
        // tile, or past every window at the screen's far corner.
        let mut park = (width - 1, height - 1);
        if let (true, Some(slot), Some(page)) = (cols * rows >= 2, free, self.pages.get(&handle)) {
            let (left, top) = ((slot % cols) * TILE_W, (slot / cols) * TILE_H);
            // GTK sends the title on its own connection; make sure the
            // server has it before looking the window up by it.
            gtk::prelude::WidgetExt::display(&page.window).sync();
            if input.place_window(&title, left, top) {
                page.origin.set((f64::from(left), f64::from(top)));
                self.tiles.insert(slot, handle);
                park = (left + TILE_W - 1, top + TILE_H - 1);
            }
        }
        // The page's own pointer (where the display has several) starts
        // off its window, so the page does not open hovered.
        input.use_page_pointer(&title);
        let _ = input.dispatch(super::wpe_input::InputEvent::Move(
            super::wpe_input::ScreenPoint {
                x: park.0,
                y: park.1,
            },
        ));
        park_core_pointer(input, (width, height));
    }

    fn page_mut(&mut self, h: PageHandle) -> EngineResult<&mut WpePage> {
        self.sync_popups();
        self.pages.get_mut(&h).ok_or(EngineError::NotFound {
            kind: "page",
            id: h.0.to_string(),
        })
    }
}

// =============================================================================
// Run-loop helper: pump the GLib main context until a predicate fires
// =============================================================================

fn run_loop_until<F: FnMut() -> bool>(mut done: F, budget: Duration) -> bool {
    let main_ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + budget;
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        if done() {
            return true;
        }
        if main_ctx.iteration(false) {
            continue;
        }
        // Nothing ready: block until a source fires or the slice ends.
        // A fixed sleep here noticed every eval result and input event
        // up to 10ms late.
        let left = deadline.duration_since(now).min(Duration::from_millis(50));
        let fired = std::rc::Rc::new(Cell::new(false));
        let flag = fired.clone();
        let timer = glib::timeout_add_local_once(left, move || flag.set(true));
        main_ctx.iteration(true);
        if !fired.get() {
            timer.remove();
        }
    }
    done()
}

impl WpeBackend {
    /// Drop a new-window request parked before this input primitive
    /// started (a popup the page fired on its own), so the primitive
    /// only follows what it caused.
    fn forget_new_window(&mut self, page: PageHandle) -> EngineResult<()> {
        let _ = self.page_mut(page)?.new_window.borrow_mut().take();
        let mut s = self.popups.borrow_mut();
        s.raised_at_input = s.feed.raised();
        Ok(())
    }

    /// Load a new-window navigation the last input primitive caused, in
    /// this page, and wait for it like `goto`. The `create` signal can
    /// arrive after the click returns, so after a click (`settle`) pump
    /// for up to `NEW_WINDOW_SETTLE` first. No parked request: no-op.
    fn follow_new_window(&mut self, page: PageHandle, settle: bool) -> EngineResult<()> {
        let popups = self.popups.clone();
        let p = self.page_mut(page)?;
        if settle && p.new_window.borrow().is_none() {
            let hint = eval_js_string(
                &p.web_view,
                super::common::NEW_WINDOW_HINT_JS,
                Duration::from_secs(2),
            );
            if super::common::new_window_hinted(&hint) {
                if p.popups_on.get() {
                    // A real window: wait for WebKit to ask for it, so
                    // the popup exists when the click returns.
                    let (feed, before) = {
                        let s = popups.borrow();
                        (s.feed.clone(), s.raised_at_input)
                    };
                    let _ = run_loop_until(
                        move || feed.raised() > before,
                        super::common::NEW_WINDOW_SETTLE,
                    );
                    return Ok(());
                }
                let parked = p.new_window.clone();
                let _ = run_loop_until(
                    move || parked.borrow().is_some(),
                    super::common::NEW_WINDOW_SETTLE,
                );
            }
        }
        let Some(request) = p.new_window.borrow_mut().take() else {
            return Ok(());
        };
        let web_view = p.web_view.clone();
        web_view.load_request(&request);
        wait_for_load(&web_view, "act")
    }
}
