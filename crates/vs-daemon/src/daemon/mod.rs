//! The [`Daemon`] — the brain of the vibesurfer daemon.
//!
//! Owns the SQLite store, the engine runtime, and the in-memory session
//! cache. Each primitive is a `pub fn` on [`Daemon`] living in one of
//! the per-group submodules ([`lifecycle`], [`page_ops`], [`store_ops`],
//! [`engine_ops`], [`inspect`], [`input`], [`human`]); shared helpers
//! (audit, session lookup, key resolution) plus the [`Daemon`] struct
//! and builders live here.
//!
//! Concurrency: every public method is `&self` and acquires
//! fine-grained locks on the session map. Engine calls are dispatched
//! onto the engine thread via `EngineRuntime`; the daemon's own state
//! is protected by `std::sync::Mutex`.

mod audit;
pub mod responses;

mod engine_ops;
mod human;
mod input;
mod inspect;
mod lifecycle;
mod net_ops;
mod page_ops;
pub mod pending;
pub(crate) mod popups;
pub(crate) mod reaper;
mod record;
mod store_ops;
pub mod webentry;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;
use vs_engine_webkit::EngineRuntime;
use vs_protocol::{Node, StateToken};
use vs_store::{ActionInsert, Store};

use crate::error::{DaemonError, Result};
use crate::page_state::PageState;

pub(crate) use audit::AuditCtx;
pub use responses::{
    ActCall, ActResponse, AnnotateResponse, AuthClearResponse, AuthListResponse, AuthLoadResponse,
    AuthSaveResponse, CaptureResponse, CloseResponse, DownloadListResponse, DownloadResponse,
    ExtractResponse, FindHit, FindResponse, LayoutResponse, LogResponse, MarkResponse,
    OpenResponse, ReadResponse, SessionCloseResponse, SessionOpenResponse, SkillListResponse,
    SkillShowResponse, StatusResponse, ViewResponse, ViewportResponse, WaitResponse,
};

/// One in-memory session.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct SessionState {
    pub(crate) pages: HashMap<String, PageState>,
    /// When a primitive last addressed this session. The reaper reads
    /// it; a session with no pages has nothing else to go on.
    pub(crate) last_touched: Instant,
    /// Pages use a private in-memory data store keyed by the session
    /// id instead of the shared persistent one.
    pub(crate) isolated: bool,
    /// Pages go through the session's intercept proxy, in a private
    /// store of their own.
    pub(crate) intercept: bool,
    /// The running proxy. `None` for a non-intercepting session, and
    /// for an intercepting one rebuilt after a restart until its first
    /// page needs it.
    pub(crate) net: Option<Arc<crate::net::SessionNet>>,
    /// Pages open real popup windows (see `Daemon::adopt_popups`).
    pub(crate) popups: bool,
    /// The proxy does not check upstream certificates.
    pub(crate) ignore_https_errors: bool,
}

impl SessionState {
    pub(crate) fn new(isolated: bool) -> Self {
        Self {
            pages: HashMap::new(),
            last_touched: Instant::now(),
            isolated,
            intercept: false,
            net: None,
            popups: false,
            ignore_https_errors: false,
        }
    }
}

/// Shared daemon state. Cheap to clone (it's an `Arc` inside).
#[derive(Clone)]
pub struct Daemon {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) store: Mutex<Store>,
    pub(crate) engine: Arc<EngineRuntime>,
    pub(crate) sessions: Mutex<HashMap<String, SessionState>>,
    pub(crate) captures_dir: std::path::PathBuf,
    pub(crate) downloads_dir: std::path::PathBuf,
    pub(crate) skills_dir: std::path::PathBuf,
    pub(crate) master_key: Option<vs_store::MasterKey>,
    pub(crate) pending: Arc<pending::PendingQueue>,
    pub(crate) webentry: Mutex<Option<Arc<webentry::WebEntry>>>,
    pub(crate) recorders: Mutex<HashMap<String, record::RecorderHandle>>,
    /// Where the intercept CA is kept; `None` keeps it in memory.
    pub(crate) intercept_ca_dir: Option<std::path::PathBuf>,
    pub(crate) mitm: Mutex<Option<Arc<crate::net::Mitm>>>,
    /// Popup events the engine has raised and the daemon has handled;
    /// see `daemon::popups`.
    pub(crate) popups: popups::Counters,
}

impl Daemon {
    /// Build a daemon around an existing store and engine. Optional
    /// fields default to sensible values; tune via the `with_*` chain.
    #[must_use]
    pub fn new(store: Store, engine: Arc<EngineRuntime>) -> Self {
        Self {
            inner: Arc::new(Inner {
                store: Mutex::new(store),
                engine,
                sessions: Mutex::new(HashMap::new()),
                captures_dir: std::env::temp_dir().join("vibesurfer-captures"),
                downloads_dir: std::env::temp_dir().join("vibesurfer-downloads"),
                skills_dir: std::path::PathBuf::from("./skills"),
                master_key: None,
                pending: pending::PendingQueue::new(),
                webentry: Mutex::new(None),
                recorders: Mutex::new(HashMap::new()),
                intercept_ca_dir: None,
                mitm: Mutex::new(None),
                popups: popups::Counters::default(),
            }),
        }
    }

    /// Pin the on-disk path where `vs_capture` writes images.
    /// Must run before the daemon is [`Arc::clone`]d.
    #[must_use]
    pub fn with_captures_dir(self, dir: impl Into<std::path::PathBuf>) -> Self {
        let mut inner = Arc::try_unwrap(self.inner)
            .map_err(|_| ())
            .expect("Daemon::with_captures_dir must run before any clone of the daemon handle");
        inner.captures_dir = dir.into();
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Pin the on-disk path where `vs_download` writes files.
    /// Must run before the daemon is [`Arc::clone`]d.
    #[must_use]
    pub fn with_downloads_dir(self, dir: impl Into<std::path::PathBuf>) -> Self {
        let mut inner = Arc::try_unwrap(self.inner)
            .map_err(|_| ())
            .expect("Daemon::with_downloads_dir must run before any clone of the daemon handle");
        inner.downloads_dir = dir.into();
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Pin the on-disk path where `vs_skill` looks up composed skills.
    #[must_use]
    pub fn with_skills_dir(self, dir: impl Into<std::path::PathBuf>) -> Self {
        let mut inner = Arc::try_unwrap(self.inner)
            .map_err(|_| ())
            .expect("Daemon::with_skills_dir must run before any clone of the daemon handle");
        inner.skills_dir = dir.into();
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Keep the intercept CA (`intercept-ca.pem`, `intercept-ca.key`)
    /// in `dir`. Without this it lives in memory for the process.
    #[must_use]
    pub fn with_intercept_ca_dir(self, dir: impl Into<std::path::PathBuf>) -> Self {
        let mut inner = Arc::try_unwrap(self.inner)
            .map_err(|_| ())
            .expect("Daemon::with_intercept_ca_dir must run before any clone of the daemon handle");
        inner.intercept_ca_dir = Some(dir.into());
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Pin the master key used by `vs_auth save|load`. Without this,
    /// auth-modifying primitives return `BadRequest "no master key"`.
    #[must_use]
    pub fn with_master_key(self, key: vs_store::MasterKey) -> Self {
        let mut inner = Arc::try_unwrap(self.inner)
            .map_err(|_| ())
            .expect("Daemon::with_master_key must run before any clone of the daemon handle");
        inner.master_key = Some(key);
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Wrap a primitive body so that `actions` is written exactly
    /// once, regardless of `Ok`/`Err`. The closure receives `&mut
    /// AuditCtx` and may mutate it as the call learns information.
    pub(crate) fn audit_call<R, F>(&self, mut ctx: AuditCtx, f: F) -> Result<R>
    where
        F: FnOnce(&mut AuditCtx) -> Result<R>,
    {
        let started = Instant::now();
        let result = f(&mut ctx);
        let error_code = result.as_ref().err().map(|e| e.wire().0.to_string());
        self.audit_from_ctx(&ctx, started.elapsed(), error_code)?;
        result
    }

    /// Persist an audit row from a finished [`AuditCtx`].
    fn audit_from_ctx(
        &self,
        ctx: &AuditCtx,
        latency: Duration,
        error_code: Option<String>,
    ) -> Result<()> {
        let now = vs_store::epoch_secs();
        let row = ActionInsert {
            session_id: ctx.session_id.clone(),
            page_id: ctx.page_id.clone(),
            primitive: ctx.primitive.to_string(),
            args_redacted: ctx.args_redacted.clone(),
            args_hash: ctx.args_hash.clone(),
            before_token: ctx.before_token.map(|t| t.to_string()),
            after_token: ctx.after_token.map(|t| t.to_string()),
            idempotency_hit: ctx.idempotency_hit,
            result_summary: ctx.result_summary.clone(),
            latency_ms: i64::try_from(latency.as_millis()).unwrap_or(i64::MAX),
            group_label: ctx.group_label.clone(),
            started_at: now,
            finished_at: now,
            error_code,
        };
        self.inner
            .store
            .lock()
            .expect("poisoned")
            .record_action(&row)?;
        Ok(())
    }

    pub(crate) fn require_session(&self, session_id: &str) -> Result<()> {
        if !self
            .inner
            .sessions
            .lock()
            .expect("poisoned")
            .contains_key(session_id)
        {
            return Err(DaemonError::UnknownSession(session_id.to_string()));
        }
        Ok(())
    }

    pub(crate) fn require_master_key(&self) -> Result<&vs_store::MasterKey> {
        self.inner
            .master_key
            .as_ref()
            .ok_or(DaemonError::BadRequest(
                "no master key configured; daemon was not started with one".into(),
            ))
    }

    /// Error for a page missing from the addressed session: `WrongSession`
    /// if the (globally-unique) page id lives in a different session, else
    /// `UnknownPage`. Turns the misleading `NOT_FOUND page=P` into a
    /// signal the caller can act on (switch sessions).
    fn missing_page(
        sessions: &HashMap<String, SessionState>,
        addressed: &str,
        page_id: &str,
    ) -> DaemonError {
        for (sid, s) in sessions {
            if s.pages.contains_key(page_id) {
                return DaemonError::WrongSession {
                    page: page_id.to_string(),
                    addressed: addressed.to_string(),
                    page_session: sid.clone(),
                };
            }
        }
        DaemonError::UnknownPage(page_id.to_string())
    }

    /// The engine data store a new page of `session_id` opens in.
    pub(crate) fn data_store_for(&self, session_id: &str) -> Result<vs_engine_webkit::DataStore> {
        let mut sessions = self.inner.sessions.lock().expect("poisoned");
        let s = sessions
            .get_mut(session_id)
            .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?;
        if s.intercept {
            let mitm = self.mitm()?;
            return Ok(vs_engine_webkit::DataStore::Proxied {
                key: session_id.to_string(),
                port: s.running_net(self)?.port(),
                ca: mitm.ca_der().to_vec(),
                spki_pins: mitm.spki_pins(),
            });
        }
        Ok(if s.isolated {
            vs_engine_webkit::DataStore::Isolated(session_id.to_string())
        } else {
            vs_engine_webkit::DataStore::Shared
        })
    }

    pub(crate) fn engine_handle_for(
        &self,
        session_id: &str,
        page_id: &str,
    ) -> Result<vs_engine_webkit::PageHandle> {
        // Every engine-touching primitive comes through here, so this
        // is where "still in use" is recorded for the reaper.
        self.touch(session_id, Some(page_id));
        let url = {
            let sessions = self.inner.sessions.lock().expect("poisoned");
            let session = sessions
                .get(session_id)
                .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?;
            let page = session
                .pages
                .get(page_id)
                .ok_or_else(|| Self::missing_page(&sessions, session_id, page_id))?;
            if let Some(h) = page.engine_handle {
                return Ok(h);
            }
            (page.url.clone(), page.init_scripts.clone())
        };
        let (url, scripts) = url;
        let store = self.data_store_for(session_id)?;
        // Dormant page (resurrected from the store): create the engine
        // page now, outside the map lock — engine calls dispatch to
        // the platform main thread and must not deadlock against it.
        // With init scripts, open blank, install them, then load, so
        // the first document already has them.
        let handle = if scripts.is_empty() {
            self.inner.engine.open(&url, &store)?
        } else {
            let h = self.inner.engine.open("about:blank", &store)?;
            for s in &scripts {
                self.inner.engine.add_init_script(h, s)?;
            }
            self.inner.engine.navigate(h, &url)?;
            h
        };
        self.apply_popups(session_id, handle);
        let mut sessions = self.inner.sessions.lock().expect("poisoned");
        let page = sessions
            .get_mut(session_id)
            .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?
            .pages
            .get_mut(page_id)
            .ok_or_else(|| DaemonError::UnknownPage(page_id.to_string()))?;
        // Raced with another caller: keep theirs, close ours.
        if let Some(existing) = page.engine_handle {
            drop(sessions);
            let _ = self.inner.engine.close(handle);
            return Ok(existing);
        }
        page.engine_handle = Some(handle);
        Ok(handle)
    }

    /// Clone the per-page mutation lock so callers can hold it across
    /// an engine call without keeping the session map locked.
    pub(crate) fn mutate_lock_for(
        &self,
        session_id: &str,
        page_id: &str,
    ) -> Result<Arc<Mutex<()>>> {
        let sessions = self.inner.sessions.lock().expect("poisoned");
        let page = sessions
            .get(session_id)
            .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?
            .pages
            .get(page_id)
            .ok_or_else(|| Self::missing_page(&sessions, session_id, page_id))?;
        Ok(page.mutate_lock.clone())
    }

    pub(crate) fn current_token(&self, session_id: &str, page_id: &str) -> Result<StateToken> {
        // The other choke point: store-only primitives read a token
        // without ever asking the engine for a handle.
        self.touch(session_id, Some(page_id));
        let sessions = self.inner.sessions.lock().expect("poisoned");
        let page = sessions
            .get(session_id)
            .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?
            .pages
            .get(page_id)
            .ok_or_else(|| Self::missing_page(&sessions, session_id, page_id))?;
        Ok(page.last_token.unwrap_or(StateToken::ZERO))
    }

    /// Direct read access for tests.
    #[doc(hidden)]
    pub fn audit_log(&self, filter: &vs_store::ActionFilter) -> Result<Vec<vs_store::Action>> {
        Ok(self
            .inner
            .store
            .lock()
            .expect("poisoned")
            .list_actions(filter)?)
    }

    /// Run a sequence of primitives in order. Per-primitive failures
    /// (parse errors, unknown sessions, stale tokens) produce inline
    /// error envelopes but do not abort the rest of the sequence.
    ///
    /// Audit rows are written per primitive — a sequence does not
    /// become one audit row.
    ///
    /// Today every wire frame parses to exactly one [`Primitive`]
    /// (i.e. one [`Request`](vs_protocol::Request)), so the inbound
    /// vec has length 1.
    #[must_use]
    pub fn dispatch(
        &self,
        primitives: &[crate::dispatch::Primitive],
    ) -> Vec<crate::dispatch::DispatchOutcome> {
        primitives
            .iter()
            .map(|p| crate::dispatch::DispatchOutcome::from_wire(crate::server::dispatch(self, p)))
            .collect()
    }
}

pub(crate) fn short_id() -> String {
    // Take 16 hex chars (64 bits): 12 chars of v7 ms timestamp + 4 chars
    // of (version + 12 bits of v7 rand_a). Truncating to just the
    // timestamp prefix collided whenever two ids were generated in the
    // same millisecond — annotate-twice tests were flaky for exactly
    // that reason. 64 bits is still short on the wire and far below the
    // Take 24 hex chars (96 bits) — includes 12 hex of v7 timestamp,
    // version + rand_a (12 bits), variant + ~30 bits of rand_b. The
    // earlier 16-char form was still flaky in tests because rand_a is
    // only 12 bits of randomness and uuid v7 implementations can use
    // it as a counter rather than fresh random per-call. 24 chars
    // gives us enough of rand_b that collision probability per pair is
    // ~2^-30, vanishing for any test process.
    Uuid::now_v7().simple().to_string()[..24].to_string()
}

pub(crate) fn render_subtree_text(node: &Node) -> String {
    let mut out = String::new();
    render_node_text(node, 0, &mut out);
    out
}

fn render_node_text(node: &Node, depth: usize, out: &mut String) {
    use std::fmt::Write as _;
    for _ in 0..depth {
        out.push_str("  ");
    }
    let _ = write!(out, "[{}] {}: {}", node.r, node.role, node.label);
    out.push('\n');
    for child in &node.children {
        render_node_text(child, depth + 1, out);
    }
}

/// Wire the capability gate: query the engine's current capabilities,
/// route the requested flag through `pick`, and surface
/// `EngineError::Unsupported` cleanly when the install path didn't
/// succeed for this engine instance. Used by every `vs_inspect` daemon
/// method to keep the wire honest — `! ENGINE_UNSUPPORTED <op>` flows
/// out instead of an empty buffer that lies about coverage.
pub(super) fn require_capability<F>(daemon: &Daemon, pick: F, op: &'static str) -> Result<()>
where
    F: FnOnce(&vs_engine_webkit::EngineCapabilities) -> bool,
{
    let caps = daemon.inner.engine.capabilities()?;
    if pick(&caps) {
        Ok(())
    } else {
        Err(crate::error::DaemonError::Engine(
            vs_engine_webkit::EngineError::Unsupported {
                engine: caps.name,
                primitive: op,
            },
        ))
    }
}
