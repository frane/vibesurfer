//! Page operations: `vs_view`, `vs_read`, `vs_act`, `vs_find`, `vs_wait`,
//! `vs_status`.

use std::time::Duration;

use vs_engine_webkit::WaitCondition as EngineWait;
use vs_protocol::{Ref, StateToken, Warning, WarningCode};

use super::audit::AuditCtx;
use super::responses::{
    ActCall, ActResponse, FindHit, FindResponse, ReadResponse, StatusResponse, ViewResponse,
    WaitResponse,
};
use super::{render_subtree_text, Daemon};
use crate::error::{DaemonError, Result};
use crate::tokens;

impl Daemon {
    pub fn view(&self, session_id: &str, page_id: &str, force_full: bool) -> Result<ViewResponse> {
        let ctx = AuditCtx::new("vs_view", session_id)
            .with_page(page_id)
            .with_args(String::new(), tokens::args_hash("vs_view", &[]));
        self.audit_call(ctx, |ctx| {
            let engine_handle = self.engine_handle_for(session_id, page_id)?;
            let (tree, console) = self.inner.engine.snapshot_with_console(engine_handle)?;
            let (token, form) = {
                let mut sessions = self.inner.sessions.lock().expect("poisoned");
                let page = sessions
                    .get_mut(session_id)
                    .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?
                    .pages
                    .get_mut(page_id)
                    .ok_or_else(|| DaemonError::UnknownPage(page_id.to_string()))?;
                if force_full {
                    page.invalidate_baseline();
                }
                page.apply_snapshot(tree)
            };
            ctx.after_token = Some(token);
            let mut store = self.inner.store.lock().expect("poisoned");
            store.update_page_token(page_id, &token.to_string(), "engine", None)?;
            drop(store);
            Ok(ViewResponse {
                token,
                form,
                warnings: Vec::new(),
                console,
            })
        })
    }

    pub fn read(&self, session_id: &str, page_id: &str, r: Ref) -> Result<ReadResponse> {
        let ctx = AuditCtx::new("vs_read", session_id)
            .with_page(page_id)
            .with_args(
                r.to_string(),
                tokens::args_hash("vs_read", &[r.to_string()]),
            );
        self.audit_call(ctx, |ctx| {
            let (token, body) = {
                let sessions = self.inner.sessions.lock().expect("poisoned");
                let page = sessions
                    .get(session_id)
                    .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?
                    .pages
                    .get(page_id)
                    .ok_or_else(|| DaemonError::UnknownPage(page_id.to_string()))?;
                let node = page.find_node(r).ok_or(DaemonError::UnknownRef(r.0))?;
                (
                    page.last_token.unwrap_or(StateToken::ZERO),
                    render_subtree_text(node),
                )
            };
            ctx.before_token = Some(token);
            ctx.after_token = Some(token);
            Ok(ReadResponse { token, body })
        })
    }

    pub fn act(&self, call: ActCall) -> Result<ActResponse> {
        let ActCall {
            session_id,
            page_id,
            target,
            action,
            before_token,
            args_hash,
            args_redacted,
            group_label,
            mode,
        } = call;
        let ctx = AuditCtx::new("vs_act", &session_id)
            .with_page(&page_id)
            .with_args(args_redacted, args_hash.clone())
            .with_before(before_token)
            .with_group(group_label);
        self.audit_call(ctx, |ctx| {
            // Hold the page's mutation lock across the whole
            // check-then-act window so a concurrent act with the same
            // before_token can't slip between the stale-token check
            // and the engine call.
            let mutate_lock = self.mutate_lock_for(&session_id, &page_id)?;
            let _mutate_guard = mutate_lock.lock().expect("poisoned");
            let engine_handle = self.engine_handle_for(&session_id, &page_id)?;
            let current_token = self.current_token(&session_id, &page_id)?;
            if current_token != before_token {
                return Err(DaemonError::StaleToken {
                    current: current_token,
                    reason: "mutate",
                });
            }
            let now = vs_store::epoch_secs();
            let store = self.inner.store.lock().expect("poisoned");
            let cached = store.lookup_idempotent(
                &page_id,
                &before_token.to_string(),
                &args_hash,
                now,
                vs_store::IDEMPOTENCY_TTL_SECS,
            )?;
            drop(store);

            if let Some(cached) = cached {
                let token = cached
                    .after_token
                    .as_deref()
                    .and_then(|s| s.parse::<StateToken>().ok())
                    .unwrap_or(StateToken::ZERO);
                ctx.idempotency_hit = true;
                ctx.after_token = Some(token);
                ctx.result_summary = Some("idem".into());
                return Ok(ActResponse {
                    token,
                    form: crate::page_state::ViewForm::NoChange,
                    warnings: vec![Warning::new(WarningCode::IdempotentHit)],
                });
            }

            // Acting on a ref the last snapshot marked hidden (0x0 or
            // invisible) usually means the agent picked a dead
            // duplicate — the click still dispatches (JS path), but
            // warn so the agent knows to re-aim if nothing happens.
            let mut warnings = Vec::new();
            if let vs_engine_webkit::ActTarget::Ref(r) = &target {
                let hidden = {
                    let sessions = self.inner.sessions.lock().expect("poisoned");
                    sessions
                        .get(&session_id)
                        .and_then(|s| s.pages.get(&page_id))
                        .and_then(|p| p.find_node(*r))
                        .is_some_and(|n| n.attrs.get("hid").is_some_and(|v| v == "1"))
                };
                if hidden {
                    warnings.push(Warning::with_args(
                        WarningCode::HiddenTarget,
                        vec![format!("ref={}", r.0)],
                    ));
                }
            }
            let tree = self
                .inner
                .engine
                .act_and_snapshot(engine_handle, target, action, mode)?;
            let (token, form) = {
                let mut sessions = self.inner.sessions.lock().expect("poisoned");
                let page = sessions
                    .get_mut(&session_id)
                    .ok_or_else(|| DaemonError::UnknownSession(session_id.clone()))?
                    .pages
                    .get_mut(&page_id)
                    .ok_or_else(|| DaemonError::UnknownPage(page_id.clone()))?;
                page.apply_snapshot(tree)
            };
            ctx.after_token = Some(token);

            let mut store = self.inner.store.lock().expect("poisoned");
            store.update_page_token(&page_id, &token.to_string(), "engine", None)?;
            drop(store);

            Ok(ActResponse {
                token,
                form,
                warnings,
            })
        })
    }

    pub fn find(&self, session_id: &str, query: &str) -> Result<FindResponse> {
        let ctx = AuditCtx::new("vs_find", session_id).with_args(
            query.to_string(),
            tokens::args_hash("vs_find", &[query.to_string()]),
        );
        self.audit_call(ctx, |_ctx| {
            self.require_session(session_id)?;
            let mut hits = Vec::new();
            let sessions = self.inner.sessions.lock().expect("poisoned");
            let s = sessions
                .get(session_id)
                .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?;
            for (page_id, page) in &s.pages {
                if let Some(tree) = &page.last_tree {
                    for node in tree {
                        if node.label.contains(query) {
                            hits.push(FindHit {
                                page_id: page_id.clone(),
                                r: node.r,
                                role: node.role.to_string(),
                                label: node.label.clone(),
                            });
                        }
                    }
                }
            }
            Ok(FindResponse { hits })
        })
    }

    /// Apply a fresh engine snapshot to the page, advance its delta
    /// baseline, persist the new token, and return (token, delta).
    /// Every returned token is therefore a valid pre-image and every
    /// caller advances the baseline. Shared by wait; act and cursor_op
    /// inline the same steps.
    fn advance_baseline(
        &self,
        session_id: &str,
        page_id: &str,
        tree: vs_protocol::Tree,
    ) -> Result<(StateToken, crate::page_state::ViewForm)> {
        let (token, form) = {
            let mut sessions = self.inner.sessions.lock().expect("poisoned");
            let page = sessions
                .get_mut(session_id)
                .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?
                .pages
                .get_mut(page_id)
                .ok_or_else(|| DaemonError::UnknownPage(page_id.to_string()))?;
            page.apply_snapshot(tree)
        };
        let mut store = self.inner.store.lock().expect("poisoned");
        store.update_page_token(page_id, &token.to_string(), "engine", None)?;
        drop(store);
        Ok((token, form))
    }

    pub fn wait(
        &self,
        session_id: &str,
        page_id: &str,
        cond: EngineWait,
        budget: Duration,
    ) -> Result<WaitResponse> {
        let ctx = AuditCtx::new("vs_wait", session_id)
            .with_page(page_id)
            .with_args(String::new(), tokens::args_hash("vs_wait", &[]));
        self.audit_call(ctx, |ctx| {
            let engine_handle = self.engine_handle_for(session_id, page_id)?;
            // `token-change` is daemon-anchored: it means "the state
            // token differs from the last one this page handed out",
            // not "a mutation fired after this wait started". A JS
            // MutationObserver cannot express that — it misses changes
            // that land between the action and the wait, and its state
            // dies with the document on navigation (the biggest change
            // of all). So poll real snapshots and compare real tokens.
            if matches!(cond, EngineWait::TokenChange) {
                let (baseline, url) = {
                    let sessions = self.inner.sessions.lock().expect("poisoned");
                    let page = sessions
                        .get(session_id)
                        .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?
                        .pages
                        .get(page_id)
                        .ok_or_else(|| DaemonError::UnknownPage(page_id.to_string()))?;
                    (page.last_token, page.url.clone())
                };
                let deadline = std::time::Instant::now() + budget;
                let poll = Duration::from_millis(250);
                loop {
                    let tree = self.inner.engine.snapshot(engine_handle)?;
                    let token = tokens::compute(&tree, &url, page_id);
                    if baseline != Some(token) {
                        let (token, _form) = self.advance_baseline(session_id, page_id, tree)?;
                        ctx.after_token = Some(token);
                        return Ok(WaitResponse { token });
                    }

                    if std::time::Instant::now() + poll > deadline {
                        return Err(DaemonError::Engine(
                            vs_engine_webkit::EngineError::Timeout {
                                budget,
                                primitive: "wait",
                            },
                        ));
                    }
                    std::thread::sleep(poll);
                }
            }
            self.inner.engine.wait(engine_handle, cond, budget)?;
            let tree = self.inner.engine.snapshot(engine_handle)?;
            let (token, _form) = self.advance_baseline(session_id, page_id, tree)?;
            ctx.after_token = Some(token);
            Ok(WaitResponse { token })
        })
    }

    /// Single-block summary: open pages, recent actions, total counts.
    /// `session_id_opt = None` returns a workspace-wide summary.
    pub fn status(&self, session_id_opt: Option<&str>) -> Result<StatusResponse> {
        let ctx = AuditCtx::new("vs_status", session_id_opt.unwrap_or("(none)"))
            .with_args(String::new(), tokens::args_hash("vs_status", &[]));
        self.audit_call(ctx, |_ctx| {
            let body = self.render_status(session_id_opt)?;
            Ok(StatusResponse { body })
        })
    }

    fn render_status(&self, session_id_opt: Option<&str>) -> Result<String> {
        use std::fmt::Write as _;
        let mut out = String::new();
        // Daemon identity line: version + protocol level + the
        // capability flags a client can auto-negotiate (so testsurfer
        // et al. can enable actDeltas / clickVia without pinning to a
        // hardcoded daemon version). Always first, both modes.
        writeln!(
            out,
            "daemon\tversion={}\tproto=1\tflags=actDeltas,clickVia",
            env!("CARGO_PKG_VERSION")
        )
        .ok();
        let sessions = self.inner.sessions.lock().expect("poisoned");
        if let Some(sid) = session_id_opt {
            let s = sessions
                .get(sid)
                .ok_or_else(|| DaemonError::UnknownSession(sid.to_string()))?;
            writeln!(out, "session\t{sid}\tpages={}", s.pages.len()).ok();
            for (page_id, page) in &s.pages {
                let token = page.last_token.map(|t| t.to_string()).unwrap_or_default();
                writeln!(out, "page\t{page_id}\turl={}\ttoken={token}", page.url).ok();
            }
        } else {
            writeln!(out, "workspace\tsessions={}", sessions.len()).ok();
            for (id, s) in sessions.iter() {
                writeln!(out, "session\t{id}\tpages={}", s.pages.len()).ok();
            }
        }
        Ok(out)
    }
}
