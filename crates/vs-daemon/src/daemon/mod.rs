//! The [`Daemon`] — the brain of the vibesurfer daemon.
//!
//! Owns the SQLite store, the engine runtime, and the in-memory session
//! cache. Each primitive is a `pub fn` on [`Daemon`] living in one of
//! the per-group submodules ([`lifecycle`], [`page_ops`], [`store_ops`],
//! [`engine_ops`]); shared helpers (audit, session lookup, key
//! resolution) plus the [`Daemon`] struct + builders live here.
//!
//! Concurrency: every public method is `&self` and acquires
//! fine-grained locks on the session map. Engine calls are dispatched
//! onto the engine thread via `EngineRuntime`; the daemon's own state
//! is protected by `std::sync::Mutex`.

mod audit;
pub mod responses;

mod engine_ops;
mod lifecycle;
mod page_ops;
mod store_ops;

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
    AuthSaveResponse, CaptureResponse, CloseResponse, ExtractResponse, FindHit, FindResponse,
    LayoutResponse, LogResponse, MarkResponse, OpenResponse, ReadResponse, SessionCloseResponse,
    SessionOpenResponse, SkillListResponse, SkillShowResponse, StatusResponse, ViewResponse,
    ViewportResponse, WaitResponse,
};

/// One in-memory session.
#[derive(Debug)]
pub(crate) struct SessionState {
    pub(crate) pages: HashMap<String, PageState>,
}

impl SessionState {
    pub(crate) fn new() -> Self {
        Self {
            pages: HashMap::new(),
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
    pub(crate) skills_dir: std::path::PathBuf,
    pub(crate) master_key: Option<vs_store::MasterKey>,
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
                skills_dir: std::path::PathBuf::from("./skills"),
                master_key: None,
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

    pub(crate) fn engine_handle_for(
        &self,
        session_id: &str,
        page_id: &str,
    ) -> Result<vs_engine_webkit::PageHandle> {
        let sessions = self.inner.sessions.lock().expect("poisoned");
        let session = sessions
            .get(session_id)
            .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?;
        let page = session
            .pages
            .get(page_id)
            .ok_or_else(|| DaemonError::UnknownPage(page_id.to_string()))?;
        Ok(page.engine_handle)
    }

    pub(crate) fn current_token(&self, session_id: &str, page_id: &str) -> Result<StateToken> {
        let sessions = self.inner.sessions.lock().expect("poisoned");
        let page = sessions
            .get(session_id)
            .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?
            .pages
            .get(page_id)
            .ok_or_else(|| DaemonError::UnknownPage(page_id.to_string()))?;
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

    /// Snapshot the engine's console ring buffer for `page`.
    pub fn inspect_console(
        &self,
        session_id: &str,
        page_id: &str,
    ) -> Result<Vec<vs_engine_webkit::inspector::ConsoleEntry>> {
        let ctx = AuditCtx::new("vs_inspect", session_id)
            .with_page(page_id)
            .with_args(
                "console".into(),
                crate::tokens::args_hash("vs_inspect", &["console".into()]),
            );
        self.audit_call(ctx, |ctx| {
            self.require_session(session_id)?;
            require_capability(self, |c| c.inspector_console, "vs_inspect console")?;
            let handle = self.engine_handle_for(session_id, page_id)?;
            let entries = self.inner.engine.console_entries(handle)?;
            ctx.after_token = Some(self.current_token(session_id, page_id)?);
            Ok(entries)
        })
    }

    /// Snapshot the engine's network ring buffer for `page`.
    pub fn inspect_network(
        &self,
        session_id: &str,
        page_id: &str,
    ) -> Result<Vec<vs_engine_webkit::inspector::NetworkEntry>> {
        let ctx = AuditCtx::new("vs_inspect", session_id)
            .with_page(page_id)
            .with_args(
                "network".into(),
                crate::tokens::args_hash("vs_inspect", &["network".into()]),
            );
        self.audit_call(ctx, |ctx| {
            self.require_session(session_id)?;
            require_capability(self, |c| c.inspector_network, "vs_inspect network")?;
            let handle = self.engine_handle_for(session_id, page_id)?;
            let entries = self.inner.engine.network_entries(handle)?;
            ctx.after_token = Some(self.current_token(session_id, page_id)?);
            Ok(entries)
        })
    }
    /// Look up the full detail (headers + bodies) for a captured
    /// network request by `seq`.
    pub fn inspect_request(
        &self,
        session_id: &str,
        page_id: &str,
        seq: u64,
    ) -> Result<Option<vs_engine_webkit::inspector::RequestDetail>> {
        let ctx = AuditCtx::new("vs_inspect", session_id)
            .with_page(page_id)
            .with_args(
                format!("request {seq}"),
                crate::tokens::args_hash("vs_inspect", &["request".into(), seq.to_string()]),
            );
        self.audit_call(ctx, |ctx| {
            self.require_session(session_id)?;
            require_capability(self, |c| c.inspector_network, "vs_inspect request")?;
            let handle = self.engine_handle_for(session_id, page_id)?;
            let detail = self.inner.engine.request_detail(handle, seq)?;
            ctx.after_token = Some(self.current_token(session_id, page_id)?);
            Ok(detail)
        })
    }

    pub fn inspect_eval(
        &self,
        session_id: &str,
        page_id: &str,
        expr: &str,
    ) -> Result<vs_engine_webkit::inspector::EvalResult> {
        let redacted_expr = crate::redact::redact_string(expr);
        let ctx = AuditCtx::new("vs_inspect", session_id)
            .with_page(page_id)
            .with_args(
                format!("eval {redacted_expr}"),
                crate::tokens::args_hash("vs_inspect", &["eval".into(), redacted_expr.clone()]),
            );
        self.audit_call(ctx, |ctx| {
            self.require_session(session_id)?;
            let handle = self.engine_handle_for(session_id, page_id)?;
            let r = self.inner.engine.eval_js(handle, expr)?;
            ctx.after_token = Some(self.current_token(session_id, page_id)?);
            Ok(r)
        })
    }

    pub fn inspect_storage(
        &self,
        session_id: &str,
        page_id: &str,
        scope: vs_engine_webkit::inspector::StorageScope,
    ) -> Result<Vec<vs_engine_webkit::inspector::StorageEntry>> {
        let ctx = AuditCtx::new("vs_inspect", session_id)
            .with_page(page_id)
            .with_args(
                format!("storage {}", scope.as_str()),
                crate::tokens::args_hash("vs_inspect", &["storage".into(), scope.as_str().into()]),
            );
        self.audit_call(ctx, |ctx| {
            self.require_session(session_id)?;
            let handle = self.engine_handle_for(session_id, page_id)?;
            let entries = self.inner.engine.storage(handle, scope)?;
            ctx.after_token = Some(self.current_token(session_id, page_id)?);
            Ok(entries)
        })
    }

    pub fn inspect_scripts(
        &self,
        session_id: &str,
        page_id: &str,
    ) -> Result<Vec<vs_engine_webkit::inspector::ScriptEntry>> {
        let ctx = AuditCtx::new("vs_inspect", session_id)
            .with_page(page_id)
            .with_args(
                "scripts".into(),
                crate::tokens::args_hash("vs_inspect", &["scripts".into()]),
            );
        self.audit_call(ctx, |ctx| {
            self.require_session(session_id)?;
            let handle = self.engine_handle_for(session_id, page_id)?;
            let entries = self.inner.engine.scripts(handle)?;
            ctx.after_token = Some(self.current_token(session_id, page_id)?);
            Ok(entries)
        })
    }

    pub fn inspect_script_source(
        &self,
        session_id: &str,
        page_id: &str,
        seq: u64,
    ) -> Result<Option<vs_engine_webkit::inspector::ScriptSource>> {
        let ctx = AuditCtx::new("vs_inspect", session_id)
            .with_page(page_id)
            .with_args(
                format!("script {seq}"),
                crate::tokens::args_hash("vs_inspect", &["script".into(), seq.to_string()]),
            );
        self.audit_call(ctx, |ctx| {
            self.require_session(session_id)?;
            let handle = self.engine_handle_for(session_id, page_id)?;
            let src = self.inner.engine.script_source(handle, seq)?;
            ctx.after_token = Some(self.current_token(session_id, page_id)?);
            Ok(src)
        })
    }

    pub fn inspect_dom(
        &self,
        session_id: &str,
        page_id: &str,
        r: vs_protocol::Ref,
        extra_props: Vec<String>,
    ) -> Result<Option<vs_engine_webkit::inspector::DomDetail>> {
        let ctx = AuditCtx::new("vs_inspect", session_id)
            .with_page(page_id)
            .with_args(
                format!("dom {}", r.0),
                crate::tokens::args_hash("vs_inspect", &["dom".into(), r.0.to_string()]),
            );
        self.audit_call(ctx, |ctx| {
            self.require_session(session_id)?;
            let handle = self.engine_handle_for(session_id, page_id)?;
            let d = self.inner.engine.dom(handle, r, extra_props)?;
            ctx.after_token = Some(self.current_token(session_id, page_id)?);
            Ok(d)
        })
    }

    pub fn inspect_performance(
        &self,
        session_id: &str,
        page_id: &str,
    ) -> Result<vs_engine_webkit::inspector::PerformanceMetrics> {
        let ctx = AuditCtx::new("vs_inspect", session_id)
            .with_page(page_id)
            .with_args(
                "performance".into(),
                crate::tokens::args_hash("vs_inspect", &["performance".into()]),
            );
        self.audit_call(ctx, |ctx| {
            self.require_session(session_id)?;
            let handle = self.engine_handle_for(session_id, page_id)?;
            let m = self.inner.engine.performance(handle)?;
            ctx.after_token = Some(self.current_token(session_id, page_id)?);
            Ok(m)
        })
    }
    /// order; per-primitive failures (parse errors, unknown sessions,
    /// stale tokens, etc.) produce inline error envelopes but do not
    /// abort the rest of the sequence.
    ///
    /// Audit rows are written per primitive — a sequence does not
    /// become one audit row.
    ///
    /// Today every wire frame parses to exactly one [`Primitive`]
    /// (i.e. one [`Request`](vs_protocol::Request)), so the inbound
    /// vec has length 1. Composite-flag primitives (PRs 2–6 of
    /// M5.5) and the v2 wire pipeline syntax (ADR 0007) both feed
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
fn require_capability<F>(daemon: &Daemon, pick: F, op: &'static str) -> Result<()>
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
