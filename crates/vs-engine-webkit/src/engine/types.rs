//! Plain data the [`Engine`] trait speaks: page handles,
//! actions and input options, viewports, downloads, capabilities,
//! data stores and the engine error type.

use std::time::Duration;

use vs_protocol::{Op, Ref};

#[cfg(doc)]
use super::Engine;

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
    /// Click at `(x, y)` with another button, a click count above one
    /// or modifier keys held: native events throughout, so the page
    /// sees trusted `mousedown`/`mouseup`/`click` (`auxclick`,
    /// `contextmenu`, `dblclick`) with `detail`, `button` and the
    /// modifier flags set.
    Click { x: f64, y: f64, opts: ClickOpts },
    /// Move to `(x, y)`, then turn the wheel by `(dx, dy)` CSS px
    /// (positive `dy` scrolls down, positive `dx` right), as a native
    /// wheel event: the page sees a trusted `wheel` and scrolls the
    /// element under the pointer.
    Wheel { x: f64, y: f64, dx: f64, dy: f64 },
}

pub use crate::backend::auth::CookieData;

/// What [`Engine::cookies`] does to the jar.
#[derive(Debug, Clone)]
pub enum CookieOp {
    List,
    /// Add these, replacing any with the same name, domain and path.
    Set(Vec<CookieData>),
    /// Delete every cookie.
    Clear,
}

/// What [`Engine::drop_data`] drops onto a page: files from disk and
/// string items keyed by MIME type, as a drag from another application
/// would carry them.
#[derive(Debug, Clone, Default)]
pub struct DropData {
    pub files: Vec<std::path::PathBuf>,
    pub items: Vec<(String, String)>,
}

/// Which mouse button a [`CursorOp::Click`] presses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Modifier keys held during a [`CursorOp::Click`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

impl Modifiers {
    /// `shift,ctrl,alt,meta` in any order and subset (`control`,
    /// `option`, `cmd`/`command` accepted); `None` for an unknown name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let mut m = Self::default();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "shift" => m.shift = true,
                "ctrl" | "control" => m.ctrl = true,
                "alt" | "option" => m.alt = true,
                "meta" | "cmd" | "command" => m.meta = true,
                _ => return None,
            }
        }
        Some(m)
    }

    #[must_use]
    pub fn any(self) -> bool {
        self.shift || self.ctrl || self.alt || self.meta
    }
}

/// Button, count and modifiers of a [`CursorOp::Click`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickOpts {
    pub button: MouseButton,
    /// 1 for a click, 2 for a double click, 3 for a triple click.
    pub count: u8,
    pub modifiers: Modifiers,
}

/// Input humanization mode. Wire form: `--mode={human,careful,robotic}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Human,
    Careful,
    Robotic,
}

impl From<InputMode> for vs_humanize::InputMode {
    fn from(mode: InputMode) -> Self {
        match mode {
            InputMode::Human => Self::Human,
            InputMode::Careful => Self::Careful,
            InputMode::Robotic => Self::Robotic,
        }
    }
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
