//! The [`Engine`] trait — the daemon's view of "a browser".
//!
//! The trait is small, synchronous, and platform-agnostic. The daemon
//! drives an `Engine` from a Tokio context but never directly: every
//! call is dispatched onto the engine's dedicated thread (see
//! [`crate::runtime`]), where the platform-specific WebKit port runs.
//!
//! For the live wire types — [`Tree`], [`Node`], [`Ref`], [`Role`],
//! [`Op`] — see [`vs_protocol`].

mod popups;
mod types;

use std::path::PathBuf;
use std::time::Duration;

use vs_protocol::{Ref, Tree};

pub use popups::{Dialog, DialogAnswer, Popup, PopupEvent, PopupFeed, DIALOG_TIMEOUT};
pub use types::{
    ActTarget, Action, AuthBlob, CaptureScope, ClickOpts, CookieData, CookieOp, CursorOp,
    DataStore, Download, DownloadEntry, DownloadSource, DropData, EngineCapabilities, EngineError,
    EngineResult, InputMode, LayoutBox, Modifiers, MouseButton, OpenStart, PageHandle, RecFrame,
    Viewport, WaitCondition, DEFAULT_USER_AGENT,
};

/// The browser engine, as the daemon sees it.
///
/// All methods are synchronous from the daemon's perspective. The
/// [`runtime`](crate::runtime) layer dispatches calls onto a dedicated
/// thread that owns the platform run loop; implementations should
/// assume they are running on that thread.
pub trait Engine {
    /// Open a fresh page navigated to `url`, in data store `store`.
    fn open(&mut self, url: &str, store: &DataStore) -> EngineResult<PageHandle>;

    /// Start opening a page and return without waiting for its first
    /// load, so the engine thread serves other pages meanwhile (a new
    /// page's web process takes tens of milliseconds to start, all of
    /// it idle on this thread). The default opens synchronously.
    fn open_start(&mut self, url: &str, store: &DataStore) -> EngineResult<OpenStart> {
        self.open(url, store).map(OpenStart::Done)
    }

    /// Finish an open that [`Engine::open_start`] left pending: `None`
    /// while its load is still running, then the page or the error.
    /// Polled from the engine thread between jobs.
    fn open_poll(&mut self, _ticket: u64) -> Option<EngineResult<PageHandle>> {
        Some(Err(EngineError::Other("no open is pending".into())))
    }

    /// Discard the isolated data store keyed `key`, once its pages are
    /// closed. Unknown keys are a no-op.
    fn drop_store(&mut self, _key: &str) -> EngineResult<()> {
        Ok(())
    }

    /// Navigate an existing page to `url` in place, reusing its
    /// web view (and web-content process) instead of creating a new
    /// one. The page handle is unchanged; the document is replaced, so
    /// callers must re-baseline (all refs are fresh). Much cheaper than
    /// `open` for successive navigations.
    fn navigate(&mut self, page: PageHandle, url: &str) -> EngineResult<()>;

    /// Load `html` as the page's document under `url`: the document
    /// has that URL and origin, but nothing is fetched and no response
    /// headers apply. A test runner uses it to serve a routed main
    /// document for a host the proxy cannot see (loopback on macOS).
    fn load_html(&mut self, _page: PageHandle, _url: &str, _html: &str) -> EngineResult<()> {
        Err(EngineError::Unsupported {
            engine: self.capabilities().name,
            primitive: "vs_goto --html",
        })
    }

    /// Install a virtual WebAuthn authenticator on `page`: a pure-JS
    /// software authenticator that overrides `navigator.credentials`
    /// so passkey registration and login work headlessly, with no
    /// platform authenticator and no automation protocol. Applies to
    /// the current document and (where the backend supports it) to
    /// subsequent navigations in the same page.
    fn enable_webauthn(&mut self, page: PageHandle) -> EngineResult<()>;

    /// Run `source` at document start in every frame of every later
    /// document in `page`, before the page's own scripts. The current
    /// document is not touched (same as Playwright's addInitScript).
    /// Scripts accumulate in the order they were added and are not
    /// subject to the page's Content Security Policy.
    fn add_init_script(&mut self, page: PageHandle, source: &str) -> EngineResult<()>;

    /// Close a page. Idempotent — closing a closed page is a no-op.
    fn close(&mut self, page: PageHandle) -> EngineResult<()>;

    /// Snapshot the a11y tree at `page`.
    fn snapshot(&mut self, page: PageHandle) -> EngineResult<Tree>;

    /// Perform `action` on `target` at `page`.
    fn act(
        &mut self,
        page: PageHandle,
        target: ActTarget,
        action: Action,
        mode: InputMode,
    ) -> EngineResult<()>;

    /// Wait for `cond` at `page` until satisfied or `budget` elapses.
    fn wait(&mut self, page: PageHandle, cond: WaitCondition, budget: Duration)
        -> EngineResult<()>;

    /// Take a screenshot. Returns the path on disk where the image was
    /// written.
    fn capture(&mut self, page: PageHandle, scope: CaptureScope) -> EngineResult<PathBuf>;

    /// Capture a live frame (watch / record) at a reduced width
    /// (`max_width` px, `0` = full resolution). Cheaper to produce and
    /// encode than a full-resolution screenshot. The default delegates
    /// to [`Self::capture`]; backends that can render a smaller snapshot
    /// override it.
    fn capture_live(&mut self, page: PageHandle, _max_width: u32) -> EngineResult<PathBuf> {
        self.capture(page, CaptureScope::Viewport)
    }

    /// Begin streaming recording frames for `page`. While active, the
    /// backend captures a frame at each input step (mouse move,
    /// keystroke, click) and sends it on `tx` with the cursor position,
    /// so the recorder composites a pointer and encodes smooth motion
    /// without a poller racing the automation. `max_width` downscales
    /// each frame (`0` = full resolution). The default is unsupported so
    /// the daemon can fall back to time-based polling.
    fn record_begin(
        &mut self,
        _page: PageHandle,
        _tx: std::sync::mpsc::Sender<RecFrame>,
        _fps: u32,
        _max_width: u32,
    ) -> EngineResult<()> {
        Err(EngineError::Unsupported {
            engine: "generic",
            primitive: "record_begin",
        })
    }

    /// Stop streaming recording frames for `page` (drops the sender so
    /// the recorder's drain loop ends). Idempotent.
    fn record_end(&mut self, _page: PageHandle) -> EngineResult<()> {
        Ok(())
    }

    /// Compute layout boxes for `refs` at `page`.
    fn layout(&mut self, page: PageHandle, refs: &[Ref]) -> EngineResult<Vec<LayoutBox>>;

    /// Read a file out of `page` — either a URL fetched with the page's
    /// credentials, or a download the page itself initiated and the
    /// capture shim parked. Returns the bytes; the daemon owns where
    /// they land on disk.
    ///
    /// Headless web views have no download delegate, so a download the
    /// page starts is otherwise dropped without an event. Backends with
    /// a working JS bridge get this for free via
    /// [`common::run_download`](crate::backend); the default is
    /// unsupported.
    fn download(
        &mut self,
        _page: PageHandle,
        _source: DownloadSource,
        _budget: Duration,
    ) -> EngineResult<Download> {
        Err(EngineError::Unsupported {
            engine: self.capabilities().name,
            primitive: "download",
        })
    }

    /// List the download intents the shim has captured on `page`,
    /// payloads excluded.
    fn download_list(&mut self, _page: PageHandle) -> EngineResult<Vec<DownloadEntry>> {
        Err(EngineError::Unsupported {
            engine: self.capabilities().name,
            primitive: "download",
        })
    }

    /// Set the viewport at `page`. Triggers a re-baseline for the next
    /// `snapshot`.
    fn set_viewport(&mut self, page: PageHandle, viewport: Viewport) -> EngineResult<()>;

    /// Snapshot cookies/storage for `page` as an opaque [`AuthBlob`].
    fn save_auth(&mut self, page: PageHandle) -> EngineResult<AuthBlob>;

    /// Apply a previously saved [`AuthBlob`] to `page`.
    fn load_auth(&mut self, page: PageHandle, blob: &AuthBlob) -> EngineResult<()>;

    /// Snapshot the always-captured console ring buffer for `page`.
    /// Default impl returns empty (engines without console capture).
    fn console_entries(
        &mut self,
        _page: PageHandle,
    ) -> EngineResult<Vec<crate::inspector::ConsoleEntry>> {
        Ok(Vec::new())
    }

    /// Snapshot the always-captured network ring buffer for `page`.
    /// Default impl returns empty (engines without network capture).
    fn network_entries(
        &mut self,
        _page: PageHandle,
    ) -> EngineResult<Vec<crate::inspector::NetworkEntry>> {
        Ok(Vec::new())
    }

    /// Look up the full detail (headers + bodies) for a single
    /// captured network request. Returns `None` if the seq isn't in
    /// the buffer. Default impl returns `None`.
    fn request_detail(
        &mut self,
        _page: PageHandle,
        _seq: u64,
    ) -> EngineResult<Option<crate::inspector::RequestDetail>> {
        Ok(None)
    }

    /// Evaluate `expr` in main world. Default impl returns
    /// `EvalResult::Thrown { kind: "NotImplemented", ... }`.
    fn eval_js(
        &mut self,
        _page: PageHandle,
        _expr: &str,
    ) -> EngineResult<crate::inspector::EvalResult> {
        Ok(crate::inspector::EvalResult::Thrown {
            kind: "NotImplemented".into(),
            message: "engine does not implement vs_inspect eval".into(),
        })
    }

    /// [`Self::eval_js`] in one subframe of `page`, including a
    /// cross-origin one the page's own script cannot reach. `frame` is
    /// the frame's URL (the newest frame with it wins), or `#N` for the
    /// Nth subframe, newest first.
    fn eval_js_in_frame(
        &mut self,
        _page: PageHandle,
        _frame: &str,
        _expr: &str,
    ) -> EngineResult<crate::inspector::EvalResult> {
        Err(EngineError::Unsupported {
            engine: self.capabilities().name,
            primitive: "vs_inspect eval --frame",
        })
    }

    /// List storage entries for the requested scope.
    fn storage(
        &mut self,
        _page: PageHandle,
        _scope: crate::inspector::StorageScope,
    ) -> EngineResult<Vec<crate::inspector::StorageEntry>> {
        Ok(Vec::new())
    }

    /// List cookie store ADD/REMOVE events observed since the page
    /// opened. Engines without a cookie-changed observer (Windows, the
    /// stub) return an empty vec or `ENGINE_UNSUPPORTED` via their
    /// capability flag.
    fn cookie_events(
        &mut self,
        _page: PageHandle,
    ) -> EngineResult<Vec<crate::inspector::CookieEvent>> {
        Ok(Vec::new())
    }

    /// Page zoom: CSS px are drawn `zoom` view px wide, so the page lays
    /// out `view width / zoom` CSS px wide (a mobile layout viewport,
    /// fit to the screen). 1.0 is normal.
    fn set_zoom(&mut self, _page: PageHandle, _zoom: f64) -> EngineResult<()> {
        Err(EngineError::Unsupported {
            engine: "unknown",
            primitive: "zoom",
        })
    }

    /// Drop `data` onto `page` at view point (`x`, `y`) as a native drag
    /// from another application: the page sees trusted `dragenter`,
    /// `dragover` and, when its `dragover` accepts, `drop` (otherwise
    /// `dragleave`). Returns whether the drop was accepted.
    fn drop_data(
        &mut self,
        _page: PageHandle,
        _x: f64,
        _y: f64,
        _data: &DropData,
    ) -> EngineResult<bool> {
        Err(EngineError::Unsupported {
            engine: "unknown",
            primitive: "drop",
        })
    }

    /// Kill `page`'s web content process, to test crash handling.
    fn crash(&mut self, _page: PageHandle) -> EngineResult<()> {
        Err(EngineError::Unsupported {
            engine: "unknown",
            primitive: "crash",
        })
    }

    /// Read, add to or empty the cookie jar `page` uses (its data
    /// store: every page of an isolated session shares it), HttpOnly
    /// cookies included. Returns the jar after the change.
    fn cookies(&mut self, _page: PageHandle, _op: CookieOp) -> EngineResult<Vec<CookieData>> {
        Err(EngineError::Unsupported {
            engine: "unknown",
            primitive: "cookies",
        })
    }

    /// Trusted keyboard typing into the focused element. Sends real
    /// per-character key events (KeyDown/KeyUp) so the page sees
    /// isTrusted=true keydown → beforeinput → input, which
    /// rich-text editors (DraftJS/ProseMirror/contenteditable) and
    /// framework-controlled inputs require — the prototype-setter
    /// `fill` path bypasses their change pipeline. The caller places
    /// the caret first (e.g. `vs_click_at`). Backends without native
    /// keyboard dispatch fall through to `ENGINE_UNSUPPORTED`.
    fn type_text(&mut self, _page: PageHandle, _text: &str, _mode: InputMode) -> EngineResult<()> {
        Err(EngineError::Unsupported {
            engine: self.capabilities().name,
            primitive: "type_text",
        })
    }

    /// Coordinate-addressed cursor operation. Backends that don't
    /// implement native input dispatch fall through to the default
    /// `ENGINE_UNSUPPORTED` here; agents see `! ENGINE_UNSUPPORTED`
    /// on the wire and can switch to ref-based `vs act` instead.
    fn cursor_op(
        &mut self,
        _page: PageHandle,
        _op: CursorOp,
        _mode: InputMode,
    ) -> EngineResult<()> {
        Err(EngineError::Unsupported {
            engine: self.capabilities().name,
            primitive: "cursor_op",
        })
    }
    /// List scripts loaded by the page.
    fn scripts(&mut self, _page: PageHandle) -> EngineResult<Vec<crate::inspector::ScriptEntry>> {
        Ok(Vec::new())
    }

    /// Source of one script by `seq`. Returns `None` if the seq isn't
    /// in the script registry.
    fn script_source(
        &mut self,
        _page: PageHandle,
        _seq: u64,
    ) -> EngineResult<Option<crate::inspector::ScriptSource>> {
        Ok(None)
    }

    /// Outer HTML + computed styles for a ref. Caller-requested
    /// computed properties land in `extra_props`; the engine merges
    /// them with its default property set. Returns `None` for unknown
    /// refs.
    fn dom(
        &mut self,
        _page: PageHandle,
        _r: vs_protocol::Ref,
        _extra_props: &[String],
    ) -> EngineResult<Option<crate::inspector::DomDetail>> {
        Ok(None)
    }

    /// Web Vitals + heap + DOM stats. Returns zero-filled defaults if
    /// the engine can't measure them.
    fn performance(
        &mut self,
        _page: PageHandle,
    ) -> EngineResult<crate::inspector::PerformanceMetrics> {
        Ok(crate::inspector::PerformanceMetrics::default())
    }

    /// HTTP response of the latest main-frame navigation. `None` when
    /// the engine has not recorded one (non-HTTP load, failed load,
    /// engine without support).
    fn main_response(
        &mut self,
        _page: PageHandle,
    ) -> EngineResult<Option<crate::inspector::MainResponse>> {
        Ok(None)
    }

    /// Give `page` real popup windows: a `target="_blank"` link or
    /// form, or `window.open()`, opens a new engine page with an
    /// `opener`, instead of loading in `page`. Popups of a popup
    /// inherit the setting. Engines without support keep loading in
    /// place.
    fn set_popups(&mut self, _page: PageHandle, _on: bool) -> EngineResult<()> {
        Ok(())
    }

    /// Where pages with real popups report windows they open and
    /// pages that close themselves. Readable from any thread, so the
    /// daemon learns of a popup without waiting for the engine thread,
    /// which may be busy with the click that opened it. The engine
    /// handle in an event is usable at once.
    fn popup_feed(&mut self) -> Option<std::sync::Arc<PopupFeed>> {
        None
    }

    /// Capabilities the daemon should advertise for this engine.
    fn capabilities(&self) -> EngineCapabilities;
}

/// The role a node in `Node` would carry — re-exported for convenience.
pub use vs_protocol::Role as NodeRole;

#[cfg(test)]
mod tests {
    use super::{DialogAnswer, PopupFeed, DEFAULT_USER_AGENT};

    #[test]
    fn a_dialog_takes_one_answer_while_open() {
        let feed = PopupFeed::default();
        let yes = || DialogAnswer {
            accept: true,
            text: None,
        };
        assert!(!feed.answer(7, yes()), "unknown id");
        let id = feed.next_dialog_id();
        assert_eq!(feed.settle(id, false), None, "waits for an answer");
        assert!(feed.answer(id, yes()));
        assert!(!feed.answer(id, yes()), "second answer");
        assert_eq!(feed.settle(id, false), Some(Some(yes())));
        assert!(!feed.answer(id, yes()), "closed");
        let late = feed.next_dialog_id();
        assert_eq!(feed.settle(late, true), Some(None), "dismissed on expiry");
        assert!(!feed.answer(late, yes()), "answer after the dismiss");
    }

    /// Pins the UA contract: must look like a current shipping
    /// Safari, including the `Version/X` and `Safari/Y` suffixes.
    /// The whole reason this constant exists is that the WKWebView
    /// default UA drops both — sites that fingerprint UAs (Google,
    /// Cloudflare, etc.) flag every request without them.
    ///
    /// If this test fails because you bumped the version, that's
    /// fine — update the literal. If it fails because the format
    /// changed, think hard before relaxing it.
    #[test]
    fn default_user_agent_has_safari_suffix() {
        let ua = DEFAULT_USER_AGENT;
        assert!(
            ua.starts_with("Mozilla/5.0 "),
            "UA should start with `Mozilla/5.0 `; got {ua:?}",
        );
        assert!(
            ua.contains("AppleWebKit/"),
            "UA should advertise WebKit; got {ua:?}",
        );
        assert!(
            ua.contains("Version/"),
            "UA missing `Version/X` — anti-bot will flag it; got {ua:?}",
        );
        assert!(
            ua.contains("Safari/"),
            "UA missing `Safari/X` — anti-bot will flag it; got {ua:?}",
        );
        // Single-line — extra whitespace inside is fine, but no
        // newlines / tabs.
        assert!(
            !ua.contains('\n') && !ua.contains('\r') && !ua.contains('\t'),
            "UA must be a single line; got {ua:?}",
        );
    }
}
