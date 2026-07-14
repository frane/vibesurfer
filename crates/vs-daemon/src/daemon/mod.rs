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
pub mod pending;
pub mod webentry;
mod store_ops;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;
use vs_engine_webkit::{ActTarget as EngineActTarget, Action as EngineAction, EngineRuntime};
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
    pub(crate) pending: Arc<pending::PendingQueue>,
    pub(crate) webentry: Mutex<Option<Arc<webentry::WebEntry>>>,
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
                pending: pending::PendingQueue::new(),
                webentry: Mutex::new(None),
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
            .ok_or_else(|| Self::missing_page(&sessions, session_id, page_id))?;
        Ok(page.engine_handle)
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

    pub fn inspect_cookie_events(
        &self,
        session_id: &str,
        page_id: &str,
    ) -> Result<Vec<vs_engine_webkit::inspector::CookieEvent>> {
        let ctx = AuditCtx::new("vs_inspect", session_id)
            .with_page(page_id)
            .with_args(
                "cookie-events".to_string(),
                crate::tokens::args_hash("vs_inspect", &["cookie-events".into()]),
            );
        self.audit_call(ctx, |ctx| {
            self.require_session(session_id)?;
            let handle = self.engine_handle_for(session_id, page_id)?;
            let events = self.inner.engine.cookie_events(handle)?;
            ctx.after_token = Some(self.current_token(session_id, page_id)?);
            Ok(events)
        })
    }

    pub fn cursor_op(
        &self,
        session_id: &str,
        page_id: &str,
        op: vs_engine_webkit::engine::CursorOp,
        mode: vs_engine_webkit::engine::InputMode,
    ) -> Result<vs_protocol::StateToken> {
        let ctx = AuditCtx::new("vs_cursor_op", session_id)
            .with_page(page_id)
            .with_args(
                format!("{op:?} mode={}", mode.as_str()),
                crate::tokens::args_hash(
                    "vs_cursor_op",
                    &[format!("{op:?}"), mode.as_str().into()],
                ),
            );
        self.audit_call(ctx, |ctx| {
            self.require_session(session_id)?;
            let handle = self.engine_handle_for(session_id, page_id)?;
            self.inner.engine.cursor_op(handle, op, mode)?;
            let token = self.current_token(session_id, page_id)?;
            ctx.after_token = Some(token);
            Ok(token)
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

    // ----- Pending-input queue (v0.1.12 MCP path for vs_prompt_input) -----

    /// Enqueue a `vs_prompt_input` request and block (up to `timeout`)
    /// until the user fulfills it via `vs pending fulfill`. On
    /// fulfillment, runs the actual `vs_act fill` and returns the new
    /// state token. On cancel / timeout returns `BadRequest`.
    #[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
    pub fn prompt_input_queue(
        &self,
        session_id: &str,
        page_id: &str,
        r: vs_protocol::Ref,
        message: String,
        secret: bool,
        token: String,
        group: Option<String>,
        timeout: std::time::Duration,
    ) -> Result<StateToken> {
        let id = pending::new_id();
        let entry = pending::PendingEntry {
            id: id.clone(),
            page: page_id.to_string(),
            r: r.0,
            message,
            secret,
            token: token.clone(),
            group: group.clone(),
            form: None,
            form_index: 0,
            created_at: std::time::Instant::now(),
        };
        let value = self
            .inner
            .pending
            .enqueue_and_wait(entry, timeout)
            .ok_or_else(|| {
                DaemonError::BadRequest(format!(
                    "vs_prompt_input: pending entry {id} cancelled or timed out"
                ))
            })?;
        let before_token: StateToken = token.parse().map_err(|_| {
            DaemonError::BadRequest("vs_prompt_input: bad token (not hex 16)".into())
        })?;
        let call = ActCall {
            session_id: session_id.to_string(),
            page_id: page_id.to_string(),
            target: EngineActTarget::Ref(r),
            action: EngineAction::Fill { value },
            before_token,
            args_hash: crate::tokens::args_hash("vs_act", &["fill".into(), "***".into()]),
            args_redacted: "fill ***".into(),
            group_label: group,
        };
        let resp = self.act(call)?;
        Ok(resp.token)
    }

    /// Mint a browser entry URL for the pending queue, starting the
    /// loopback web surface on first use. The page at the URL renders
    /// every pending entry as a form; submitting fulfills them.
    pub fn web_entry_url(&self) -> Result<String> {
        let mut guard = self.inner.webentry.lock().unwrap();
        let surface = if let Some(s) = guard.as_ref() {
            s.clone()
        } else {
            let s = webentry::WebEntry::start(self.inner.pending.clone())?;
            *guard = Some(s.clone());
            s
        };
        Ok(surface.mint())
    }

    /// Enqueue a multi-field prompt form without parking. Returns the
    /// form id (for `vs_prompt_form_wait`) and a browser entry URL.
    /// Fields are `(ref, label, secret)` in fill order.
    #[allow(clippy::needless_pass_by_value)]
    pub fn prompt_form_enqueue(
        &self,
        page_id: &str,
        fields: Vec<(vs_protocol::Ref, String, bool)>,
        token: &str,
        group: Option<String>,
    ) -> Result<(String, String)> {
        if fields.is_empty() {
            return Err(DaemonError::BadRequest("vs_prompt_form: no fields".into()));
        }
        let form_id = pending::new_form_id();
        for (i, (r, label, secret)) in fields.into_iter().enumerate() {
            self.inner.pending.enqueue(pending::PendingEntry {
                id: pending::new_id(),
                page: page_id.to_string(),
                r: r.0,
                message: label,
                secret,
                token: token.to_string(),
                group: group.clone(),
                form: Some(form_id.clone()),
                form_index: u32::try_from(i).unwrap_or(u32::MAX),
                created_at: std::time::Instant::now(),
            });
        }
        let url = self.web_entry_url()?;
        Ok((form_id, url))
    }

    /// Park until every field of `form_id` is fulfilled (browser
    /// submit, or `vs pending fulfill` per entry), then fill each ref
    /// in declaration order and return the final state token. The
    /// first fill validates against the token captured at enqueue;
    /// later fills chain the token forward.
    pub fn prompt_form_wait(
        &self,
        session_id: &str,
        form_id: &str,
        timeout: std::time::Duration,
    ) -> Result<StateToken> {
        let values = self
            .inner
            .pending
            .wait_form(form_id, timeout)
            .ok_or_else(|| {
                DaemonError::BadRequest(format!(
                    "vs_prompt_form: form {form_id} cancelled, timed out, or unknown"
                ))
            })?;
        let mut token: Option<StateToken> = None;
        for (entry, value) in values {
            let before_token: StateToken = match token {
                Some(t) => t,
                None => entry.token.parse().map_err(|_| {
                    DaemonError::BadRequest("vs_prompt_form: bad token (not hex 16)".into())
                })?,
            };
            let resp = self.act(ActCall {
                session_id: session_id.to_string(),
                page_id: entry.page.clone(),
                target: EngineActTarget::Ref(vs_protocol::Ref(entry.r)),
                action: EngineAction::Fill { value },
                before_token,
                args_hash: crate::tokens::args_hash("vs_act", &["fill".into(), "***".into()]),
                args_redacted: "fill ***".into(),
                group_label: entry.group.clone(),
            })?;
            token = Some(resp.token);
        }
        token.ok_or_else(|| DaemonError::BadRequest("vs_prompt_form: empty form".into()))
    }

    /// Snapshot of currently-pending prompt entries.
    #[must_use]
    pub fn pending_list(&self) -> Vec<pending::PendingEntry> {
        self.inner.pending.list()
    }

    /// Fulfill a pending prompt entry with `value`. Returns `true` if
    /// the id was a live pending entry.
    #[must_use]
    pub fn pending_fulfill(&self, id: &str, value: String) -> bool {
        self.inner.pending.fulfill(id, value)
    }

    /// Cancel a pending prompt entry. Returns `true` if found.
    #[must_use]
    pub fn pending_cancel(&self, id: &str) -> bool {
        self.inner.pending.cancel(id)
    }

    /// Peek a pending entry (read without removing).
    #[must_use]
    pub fn pending_peek(&self, id: &str) -> Option<pending::PendingEntry> {
        self.inner.pending.peek(id)
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
