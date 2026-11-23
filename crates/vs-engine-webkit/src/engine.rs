//! The [`Engine`] trait — the daemon's view of "a browser".
//!
//! The trait is small, synchronous, and platform-agnostic. The daemon
//! drives an `Engine` from a Tokio context but never directly: every
//! call is dispatched onto the engine's dedicated thread (see
//! [`crate::runtime`]), where the platform-specific WebKit port runs.
//!
//! For the live wire types — [`Tree`], [`Node`], [`Ref`], [`Role`],
//! [`Op`] — see [`vs_protocol`].

use std::path::PathBuf;
use std::time::Duration;

use vs_protocol::{Op, Ref, Tree};

/// Default `User-Agent` the engine sets on each page. Mirrors a recent
/// shipping Safari on macOS so anti-bot fingerprinters that match on
/// the WKWebView default (which lacks the `Version/X Safari/X`
/// suffix) don't flag every request. Backends that support
/// per-webview UA (WKWebView via `setCustomUserAgent`, WebKitGTK via
/// `webkit_settings_set_user_agent`, WebView2 via `Profile2` /
/// `coreWebView2.Settings.UserAgent`) apply this string at construction.
#[cfg(target_os = "macos")]
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 \
     (KHTML, like Gecko) Version/17.5 Safari/605.1.15";

/// WebKitGTK is WebKit, so the Safari family is honest — but the OS
/// token has to say Linux. Claiming `Macintosh` here contradicted
/// `navigator.platform`, which reports the real platform: an
/// inconsistency any fingerprinter cross-checks, and one we created.
#[cfg(target_os = "linux")]
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 \
     (KHTML, like Gecko) Version/17.5 Safari/605.1.15";

/// Windows runs WebView2, which *is* Chromium. There is no honest
/// Safari string for it, and the engine's own default already names
/// the right browser and OS — see `webview2.rs`, which no longer
/// overrides it. This constant exists only so the shared code
/// compiles; nothing applies it on Windows.
#[cfg(target_os = "windows")]
pub const DEFAULT_USER_AGENT: &str = "";

/// An opaque engine-side handle to a page. The daemon associates each
/// `PageHandle` with one `pages` row in [`vs-store`](vs_protocol).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageHandle(pub u64);

/// What [`Engine::act`] targets on a page.
///
/// Only refs reach the engine. A mark is a store concept, scoped to a
/// session the engine knows nothing about, so the daemon resolves one
/// to the ref that carries it now before calling in.
#[derive(Debug, Clone)]
pub enum ActTarget {
    /// A live ref from the most recent snapshot.
    Ref(Ref),
}

/// Coordinate-addressed input operations: `vs move-to`, `vs click-at`,
/// `vs hover-at`, `vs drag`. Native event dispatch on backends that
/// support it (macOS today; Linux + Windows fall through to
/// `ENGINE_UNSUPPORTED` until M7 wires GDK / CDP input).
#[derive(Debug, Clone, Copy)]
pub enum CursorOp {
    /// Move the pointer to `(x, y)` without clicking.
    MoveTo { x: f64, y: f64 },
    /// Click at `(x, y)`. Includes a humanized lead-in from the last
    /// known cursor position when the mode is `Human`.
    ClickAt { x: f64, y: f64 },
    /// Hover (mouseover) at `(x, y)`.
    HoverAt { x: f64, y: f64 },
    /// Press at `(x1, y1)`, drag along a humanized path to `(x2, y2)`,
    /// release.
    Drag { x1: f64, y1: f64, x2: f64, y2: f64 },
}

/// Input humanization mode. Wire form: `--mode={human,careful,robotic}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Human,
    Careful,
    Robotic,
}

impl InputMode {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "human" | "h" => Some(Self::Human),
            "careful" | "c" => Some(Self::Careful),
            "robotic" | "r" => Some(Self::Robotic),
            _ => None,
        }
    }
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Careful => "careful",
            Self::Robotic => "robotic",
        }
    }
}

/// What [`Engine::act`] does at the target.
#[derive(Debug, Clone)]
pub enum Action {
    /// Activate (mouse click or `Enter`).
    Click,
    /// Replace the text content of an editable target.
    Fill { value: String },
    /// Scroll the target into view.
    Scroll,
    /// Send a key chord (`Enter`, `Ctrl+K`, …).
    Key { chord: String },
    /// Submit the enclosing form.
    Submit,
    /// Move the pointer over the target.
    Hover,
    /// Move keyboard focus.
    Focus,
}

impl Action {
    /// The protocol [`Op`](vs_protocol::Op) corresponding to this action.
    #[must_use]
    pub fn op(&self) -> Op {
        match self {
            Self::Click => Op::Click,
            Self::Fill { .. } => Op::Fill,
            Self::Scroll => Op::Scroll,
            Self::Key { .. } => Op::Key,
            Self::Submit => Op::Submit,
            Self::Hover => Op::Hover,
            Self::Focus => Op::Focus,
        }
    }
}

/// A condition for [`Engine::wait`].
#[derive(Debug, Clone)]
pub enum WaitCondition {
    /// A11y tree stable for 250ms.
    Stable,
    /// Network idle (`<= 0` in-flight requests for 500ms).
    NetIdle,
    /// `Ref(r)` appears in the tree.
    RefAppears(Ref),
    /// `Ref(r)` disappears from the tree.
    RefGone(Ref),
    /// Substring matches anywhere in the tree.
    Text(String),
    /// `state_token` changes.
    TokenChange,
}

/// Scope of a [`Engine::capture`] call.
#[derive(Debug, Clone)]
pub enum CaptureScope {
    Viewport,
    Element(Ref),
    FullPage,
}

/// A viewport definition. Sizes are in CSS pixels; `dpr` is the
/// device-pixel ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub dpr: u32,
}

impl Viewport {
    pub const MOBILE_S: Self = Self::new(320, 568, 2);
    pub const MOBILE: Self = Self::new(390, 844, 2);
    pub const MOBILE_L: Self = Self::new(414, 896, 2);
    pub const TABLET: Self = Self::new(768, 1024, 2);
    pub const TABLET_L: Self = Self::new(1024, 768, 2);
    pub const LAPTOP: Self = Self::new(1366, 768, 2);
    pub const DESKTOP: Self = Self::new(1920, 1080, 2);
    pub const DESKTOP_XL: Self = Self::new(2560, 1440, 2);
    pub const WIDE: Self = Self::new(3440, 1440, 2);

    #[must_use]
    pub const fn new(width: u32, height: u32, dpr: u32) -> Self {
        Self { width, height, dpr }
    }

    /// Look up a preset by its protocol name (e.g. `"mobile"`,
    /// `"desktop-xl"`). Returns `None` for unknown presets.
    #[must_use]
    pub fn preset(name: &str) -> Option<Self> {
        match name {
            "mobile-s" => Some(Self::MOBILE_S),
            "mobile" => Some(Self::MOBILE),
            "mobile-l" => Some(Self::MOBILE_L),
            "tablet" => Some(Self::TABLET),
            "tablet-l" => Some(Self::TABLET_L),
            "laptop" => Some(Self::LAPTOP),
            "desktop" => Some(Self::DESKTOP),
            "desktop-xl" => Some(Self::DESKTOP_XL),
            "wide" => Some(Self::WIDE),
            _ => None,
        }
    }
}

/// What [`Engine::download`] should hand back.
#[derive(Debug, Clone)]
pub enum DownloadSource {
    /// Read `url` from inside the page, with the page's own cookies and
    /// referer. Relative URLs resolve against the current document.
    Url(String),
    /// Drain the newest download the page *tried* to perform (a
    /// `download` anchor, a `blob:` navigation, a viewer's save button)
    /// and that the shim captured. `id` selects a specific buffered
    /// entry; `None` takes the newest completed one.
    Captured { id: Option<u64> },
}

/// One download the engine holds in memory, on its way to disk.
#[derive(Debug, Clone)]
pub struct Download {
    pub bytes: Vec<u8>,
    /// Suggested file name, from `Content-Disposition`, the anchor's
    /// `download` attribute, or the URL's last path segment.
    pub filename: String,
    /// MIME type as reported by the blob or the response. May be empty.
    pub mime: String,
    /// The URL the bytes came from. `blob:` for in-page saves.
    pub url: String,
}

/// A captured download intent, without its payload. Listed by
/// [`Engine::download_list`] so an agent can see what the page tried
/// to save before committing to reading any of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadEntry {
    pub id: u64,
    pub filename: String,
    pub mime: String,
    pub url: String,
    pub size: u64,
    /// `Some(msg)` when the capture failed (revoked blob, HTTP error,
    /// over the size cap). The entry is kept so the failure is visible
    /// instead of the download silently vanishing.
    pub error: Option<String>,
    pub done: bool,
}

/// Result of [`Engine::layout`]: the computed box for a single ref.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutBox {
    pub r: Ref,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub visible: bool,
    pub z_index: i32,
}

/// What an engine declares it can and cannot do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct EngineCapabilities {
    /// Renders pixels (pages are visually realized; `capture` works).
    pub renders: bool,
    /// Honors `set_viewport` requests.
    pub honors_viewport: bool,
    /// Computes layout boxes (`layout` works).
    pub measures_layout: bool,
    /// Persists and restores cookies / storage (`save_auth` / `load_auth` work).
    pub persists_auth: bool,
    /// Captures `console.*` and uncaught exceptions in a ring buffer
    /// (M5.7 PR1+). Surfaced via `vs_inspect console`.
    pub inspector_console: bool,
    /// Captures network requests in a ring buffer (M5.7 PR1+).
    /// Surfaced via `vs_inspect network`.
    pub inspector_network: bool,
    /// Captures cookie-store ADD/REMOVE events in a ring buffer
    /// (v0.1.6+). Surfaced via `vs_inspect cookie-events`. macOS and
    /// Linux only — Windows reports `false` because `webview2-com` has
    /// no cookie-changed observer.
    pub inspector_cookie_events: bool,
    /// Short identifier reported in `vs_status`.
    pub name: &'static str,
    /// Engine version string. Empty for the stub.
    pub version: &'static str,
}

impl EngineCapabilities {
    /// Capabilities advertised by the in-memory `TestEngine` (in
    /// `test_support`). Sealed behind the `test-support` Cargo
    /// feature; production builds of the daemon never see this.
    pub const TEST: Self = Self {
        renders: true,
        honors_viewport: true,
        measures_layout: true,
        persists_auth: true,
        inspector_console: true,
        inspector_network: true,
        inspector_cookie_events: true,
        name: "test",
        version: "",
    };
}

/// Opaque encrypted-by-the-engine auth payload. The daemon hands the
/// bytes to [`vs_store`](vs_protocol) for at-rest encryption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthBlob {
    pub bytes: Vec<u8>,
}

/// What can go wrong when driving an engine.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum EngineError {
    /// The engine cannot service this primitive on this platform.
    #[error("engine `{engine}` does not support `{primitive}`")]
    Unsupported {
        engine: &'static str,
        primitive: &'static str,
    },

    /// Operation exceeded its deadline.
    ///
    /// `detail` is empty for the historical two-argument wire form.
    /// Navigation timeouts say which phase died (`web process did not
    /// start` versus `navigation did not finish`) so a caller can tell
    /// a cold web process from a page that hung.
    #[error("timeout after {budget:?}: {primitive} {detail}")]
    Timeout {
        budget: Duration,
        primitive: &'static str,
        detail: &'static str,
    },

    /// The named ref or mark is not present.
    #[error("not found: {kind} {id}")]
    NotFound { kind: &'static str, id: String },

    /// The engine thread panicked or is otherwise non-responsive.
    #[error("engine thread crashed")]
    Crashed,

    /// The engine has shut down and is no longer accepting commands.
    #[error("engine has shut down")]
    Closed,

    /// Backend recognizes the primitive but has not implemented it.
    /// Distinct from [`Self::Unsupported`]: the platform *can* support
    /// it, the port just isn't there yet.
    #[error("engine `{engine}` has not implemented `{primitive}`")]
    NotImplemented {
        engine: &'static str,
        primitive: &'static str,
    },

    /// Anything else (engine-specific failure mode).
    #[error("{0}")]
    Other(String),
}

/// Convenience [`Result`] with [`EngineError`] as the error type.
pub type EngineResult<T> = std::result::Result<T, EngineError>;

/// One recorded frame streamed from the engine during automation.
///
/// `record_begin` sets up a channel; the backend then captures a frame
/// at each input step (mouse move, keystroke, click settle) and sends
/// it here without releasing the main thread. `cx`/`cy` are the cursor
/// position in *frame pixels* (already scaled to the recorded frame's
/// resolution) so the recorder can composite a pointer, and `click` is
/// a ripple phase (`1.0` at press, decaying to `0.0`; `0.0` = no ripple).
/// This is what lets a recording show real, continuous mouse motion
/// instead of a before/after slideshow: the frames are grabbed *inside*
/// the move loop, not by a poller that is blocked behind it.
pub struct RecFrame {
    pub png: Vec<u8>,
    pub cx: f64,
    pub cy: f64,
    pub click: f32,
    /// Intended elapsed time (ms) this frame represents on the input
    /// path, independent of how long the synchronous capture actually
    /// took. The recorder advances its media clock by this so motion
    /// plays at natural speed even though each snapshot inflates the real
    /// wall-clock of the move. `0` = use real elapsed time (idle frames).
    pub dt_ms: u32,
}

/// Which browser data store (cookies, localStorage, IndexedDB, cache)
/// a new page uses.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DataStore {
    /// The default persistent store every non-isolated page shares.
    Shared,
    /// A private in-memory store, keyed by the owning session's id.
    /// Pages opened with the same key share it; nothing outside that
    /// key sees it. [`Engine::drop_store`] discards it, and it never
    /// outlives the engine.
    Isolated(String),
    /// An isolated store, keyed like [`Self::Isolated`], whose network
    /// traffic goes through the HTTP proxy at `127.0.0.1:<port>`. Every
    /// engine binds proxy settings to the store (macOS data store,
    /// WebKitGTK network session, WebView2 environment), which is why a
    /// proxied page cannot share the default store.
    ///
    /// `ca` is the proxy's CA certificate (DER). The proxy answers the
    /// browser's TLS with leaves it signs, so the engine trusts it for
    /// this store's pages, and only for them. `spki_pins` lists the
    /// base64 SHA-256 of the leaf and CA public keys (comma-separated),
    /// for engines that pin keys rather than anchors.
    Proxied {
        key: String,
        port: u16,
        ca: Vec<u8>,
        spki_pins: String,
    },
}

impl DataStore {
    /// The isolation key, for the isolated and proxied variants.
    #[must_use]
    pub fn key(&self) -> Option<&str> {
        match self {
            Self::Shared => None,
            Self::Isolated(key) | Self::Proxied { key, .. } => Some(key),
        }
    }

    /// The local proxy port, for the proxied variant.
    #[must_use]
    pub fn proxy_port(&self) -> Option<u16> {
        match self {
            Self::Proxied { port, .. } => Some(*port),
            _ => None,
        }
    }

    /// The proxy's CA certificate (DER), for the proxied variant.
    #[must_use]
    pub fn proxy_ca(&self) -> Option<&[u8]> {
        match self {
            Self::Proxied { ca, .. } => Some(ca),
            _ => None,
        }
    }
}

/// How [`Engine::open_start`] went: the page is open, or its first load
/// is still running and [`Engine::open_poll`] with this ticket finishes
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenStart {
    Done(PageHandle),
    Pending(u64),
}

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
    answers: std::sync::Mutex<std::collections::HashMap<u64, DialogAnswer>>,
    last_dialog: std::sync::atomic::AtomicU64,
}

impl PopupFeed {
    /// A fresh dialog id.
    pub fn next_dialog_id(&self) -> u64 {
        self.last_dialog
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    /// Answer dialog `id`. The engine applies it on its next check
    /// (within a few milliseconds), from any thread.
    pub fn answer(&self, id: u64, answer: DialogAnswer) {
        self.answers.lock().expect("poisoned").insert(id, answer);
    }

    /// The answer for `id`, once given.
    pub fn take_answer(&self, id: u64) -> Option<DialogAnswer> {
        self.answers.lock().expect("poisoned").remove(&id)
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

/// The role a node in `Node` would carry — re-exported for convenience.
pub use vs_protocol::Role as NodeRole;

#[cfg(test)]
mod tests {
    use super::DEFAULT_USER_AGENT;

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
