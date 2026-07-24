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
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 \
     (KHTML, like Gecko) Version/17.5 Safari/605.1.15";

/// An opaque engine-side handle to a page. The daemon associates each
/// `PageHandle` with one `pages` row in [`vs-store`](vs_protocol).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageHandle(pub u64);

/// What [`Engine::act`] targets on a page.
#[derive(Debug, Clone)]
pub enum ActTarget {
    /// A live ref from the most recent snapshot.
    Ref(Ref),
    /// A persistent mark, identified by name.
    Mark(String),
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
    #[error("timeout after {budget:?}: {primitive}")]
    Timeout {
        budget: Duration,
        primitive: &'static str,
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

/// The browser engine, as the daemon sees it.
///
/// All methods are synchronous from the daemon's perspective. The
/// [`runtime`](crate::runtime) layer dispatches calls onto a dedicated
/// thread that owns the platform run loop; implementations should
/// assume they are running on that thread.
pub trait Engine {
    /// Open a fresh page navigated to `url`.
    fn open(&mut self, url: &str) -> EngineResult<PageHandle>;

    /// Navigate an existing page to `url` in place, reusing its
    /// web view (and web-content process) instead of creating a new
    /// one. The page handle is unchanged; the document is replaced, so
    /// callers must re-baseline (all refs are fresh). Much cheaper than
    /// `open` for successive navigations.
    fn navigate(&mut self, page: PageHandle, url: &str) -> EngineResult<()>;

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

    /// Compute layout boxes for `refs` at `page`.
    fn layout(&mut self, page: PageHandle, refs: &[Ref]) -> EngineResult<Vec<LayoutBox>>;

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
    /// Capabilities the daemon should advertise for this engine.
    fn capabilities(&self) -> EngineCapabilities;
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
