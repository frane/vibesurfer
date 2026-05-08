//! Inspector capture types — `ConsoleEntry`, `NetworkEntry`, ring
//! buffer + level/status enums.
//!
//! Engine implementations capture these in always-on ring buffers
//! (bounded; FIFO eviction). The agent reads them via the
//! `vs_inspect` primitive (M5.7 PR2+).
//!
//! WebKit Inspector protocol is **internal only** — it never appears
//! on the agent-facing wire. See ADR 0008.

use std::collections::VecDeque;
use std::time::SystemTime;

/// Default ring-buffer capacity per kind, per page. Configurable via
/// `Daemon::with_inspector_buffer_capacity`.
pub const DEFAULT_BUFFER_CAPACITY: usize = 1000;

/// Console levels observable by `vs_inspect console`. Mirrors the
/// browser's `console.<level>(...)` family plus the synthetic
/// `error` level we use for uncaught exceptions and unhandled
/// rejections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConsoleLevel {
    Error,
    Warn,
    Info,
    Log,
    Debug,
}

impl ConsoleLevel {
    /// Wire-format string (lowercase).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Log => "log",
            Self::Debug => "debug",
        }
    }

    /// Parse a level name (case-insensitive). Returns `None` if the
    /// name doesn't match a known level.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "error" | "err" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "log" => Some(Self::Log),
            "debug" | "trace" => Some(Self::Debug),
            _ => None,
        }
    }
}

/// One captured console event.
#[derive(Debug, Clone)]
pub struct ConsoleEntry {
    /// Wall-clock when the event was captured.
    pub timestamp: SystemTime,
    pub level: ConsoleLevel,
    /// The serialized message. Multiple `console.log("a", "b")` args
    /// are joined with a single space, matching DevTools' rendering.
    pub message: String,
    /// Optional stack trace (one frame per line). Present for
    /// uncaught exceptions and `console.error`/`trace`; `None`
    /// otherwise.
    pub stack: Option<String>,
}

/// One captured network event.
#[derive(Debug, Clone)]
pub struct NetworkEntry {
    /// Per-page sequential id, formatted on the wire as `n_<seq>`.
    pub seq: u64,
    pub timestamp: SystemTime,
    pub method: String,
    pub url: String,
    pub status: NetworkStatus,
    /// Total transfer size in bytes (response body, after
    /// decompression). 0 if not yet available.
    pub size: u64,
    /// End-to-end latency. `None` while the request is still pending.
    pub latency_ms: Option<u64>,
}

/// Status of a captured network request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkStatus {
    /// HTTP status code.
    Code(u16),
    /// Request still in flight.
    Pending,
    /// Browser aborted the request.
    Abort,
    /// CORS preflight or response policy blocked the request.
    Cors,
    /// Blocked by content-security-policy or similar.
    Blocked,
}

impl NetworkStatus {
    /// Wire-format representation.
    #[must_use]
    pub fn as_str(&self) -> String {
        match self {
            Self::Code(c) => c.to_string(),
            Self::Pending => "pending".into(),
            Self::Abort => "abort".into(),
            Self::Cors => "cors".into(),
            Self::Blocked => "blocked".into(),
        }
    }
}

/// One request/response header pair, captured for `vs_inspect request`.
#[derive(Debug, Clone)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// Full detail for one captured network request — headers + bodies for
/// both the request and response. Returned by [`crate::Engine::request_detail`]
/// when the agent calls `vs_inspect request <id>`.
#[derive(Debug, Clone)]
pub struct RequestDetail {
    pub seq: u64,
    pub method: String,
    pub url: String,
    pub status: NetworkStatus,
    pub request_headers: Vec<Header>,
    pub request_body: Option<String>,
    pub response_headers: Vec<Header>,
    pub response_body: Option<String>,
}

/// Result of `Engine::eval_js`. The `Ok` variant carries a JSON-
/// serialized value (or empty for `undefined`/`null`); errors are
/// distinguished by *kind* so the agent can react differently to a
/// thrown exception vs. a SyntaxError vs. a binding/IO failure.
#[derive(Debug, Clone)]
pub enum EvalResult {
    Ok {
        /// JSON-serialized value (or `"null"` / `"undefined"` literal).
        value: String,
        /// JS typeof string: `"string"`, `"number"`, `"object"`, etc.
        js_type: String,
    },
    /// Runtime error thrown during evaluation.
    Thrown { kind: String, message: String },
    /// Parser error — the expression was syntactically invalid.
    Syntax { message: String },
}

/// Storage scope for `Engine::storage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageScope {
    Cookies,
    Local,
    Session,
    IndexedDb,
}

impl StorageScope {
    /// Wire-format identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cookies => "cookies",
            Self::Local => "local",
            Self::Session => "session",
            Self::IndexedDb => "indexeddb",
        }
    }

    /// Parse from a wire-format identifier.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cookies" => Some(Self::Cookies),
            "local" | "localStorage" => Some(Self::Local),
            "session" | "sessionStorage" => Some(Self::Session),
            "indexeddb" | "idb" => Some(Self::IndexedDb),
            _ => None,
        }
    }
}

/// One entry returned by `Engine::storage`. Cookies use the optional
/// flag fields; key/value stores use only `key` and `value`; IndexedDB
/// uses `key` (db name) and `value` (version + object store names
/// joined). The agent doesn't need a sum type — a uniform shape keeps
/// the wire format simple.
#[derive(Debug, Clone)]
pub struct StorageEntry {
    pub key: String,
    pub value: String,
    /// Cookies only. `secure`, `httponly`, `samesite=...`, `expires=...`.
    pub flags: Vec<String>,
    /// True when the value should be redacted unless `--unsafe-log`.
    /// Cookies match the auth-header redactor; storage entries match
    /// keys like `token`, `auth`, `secret`, `key` (in their name).
    pub sensitive: bool,
}

/// One loaded script in `Engine::scripts`.
#[derive(Debug, Clone)]
pub struct ScriptEntry {
    /// Per-page sequential id, formatted on the wire as `s_<seq>`.
    pub seq: u64,
    /// Source URL, or `inline:doc[<index>]` for inline scripts.
    pub source: String,
    /// Approximate source size in bytes.
    pub size: u64,
    pub state: ScriptState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptState {
    Parsed,
    Loading,
    Error,
    BlockedCsp,
    BlockedPolicy,
}

impl ScriptState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parsed => "parsed",
            Self::Loading => "loading",
            Self::Error => "error",
            Self::BlockedCsp => "blocked-csp",
            Self::BlockedPolicy => "blocked-policy",
        }
    }
}

/// Source of one script (returned by `Engine::script_source`).
#[derive(Debug, Clone)]
pub struct ScriptSource {
    pub seq: u64,
    pub source_url: String,
    pub body: String,
}

/// Detail returned by `Engine::dom`: outer HTML + computed-style
/// pairs for one ref.
#[derive(Debug, Clone)]
pub struct DomDetail {
    pub r: u32,
    pub outer_html: String,
    /// Computed style pairs in iteration order. Default + caller-
    /// requested properties only.
    pub computed: Vec<(String, String)>,
}

/// Web Vitals + heap + DOM-stat block returned by
/// `Engine::performance`. Floats use 2 decimals on the wire; missing
/// metrics are emitted as `0` rather than dropped (keeps the line
/// shape stable).
#[derive(Debug, Clone, Default)]
pub struct PerformanceMetrics {
    pub ttfb_ms: f64,
    pub fcp_ms: f64,
    pub lcp_ms: f64,
    pub cls: f64,
    pub fid_ms: f64,
    pub long_tasks: u32,
    pub total_blocking_ms: f64,
    pub js_heap_mb: f64,
    pub dom_nodes: u32,
}

/// Bounded FIFO ring buffer for inspector captures. Adding to a full
/// buffer evicts the oldest entry. Reads return a snapshot in
/// chronological order (oldest first).
#[derive(Debug, Clone)]
pub struct RingBuffer<T> {
    items: VecDeque<T>,
    capacity: usize,
}

impl<T: Clone> RingBuffer<T> {
    /// Build a new buffer. Capacity must be > 0; capacity 0 is clamped
    /// to 1 to keep `push` infallible.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            items: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, item: T) {
        if self.items.len() == self.capacity {
            self.items.pop_front();
        }
        self.items.push_back(item);
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<T> {
        self.items.iter().cloned().collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_evicts_fifo() {
        let mut rb: RingBuffer<u32> = RingBuffer::new(3);
        for i in 0..5 {
            rb.push(i);
        }
        assert_eq!(rb.snapshot(), vec![2, 3, 4]);
        assert_eq!(rb.len(), 3);
    }

    #[test]
    fn ring_buffer_capacity_zero_is_clamped() {
        let mut rb: RingBuffer<u8> = RingBuffer::new(0);
        rb.push(1);
        assert_eq!(rb.snapshot(), vec![1]);
    }

    #[test]
    fn console_level_parse() {
        assert_eq!(ConsoleLevel::parse("error"), Some(ConsoleLevel::Error));
        assert_eq!(ConsoleLevel::parse("WARN"), Some(ConsoleLevel::Warn));
        assert_eq!(ConsoleLevel::parse("garbage"), None);
    }

    #[test]
    fn network_status_str() {
        assert_eq!(NetworkStatus::Code(200).as_str(), "200");
        assert_eq!(NetworkStatus::Pending.as_str(), "pending");
        assert_eq!(NetworkStatus::Cors.as_str(), "cors");
    }
}
