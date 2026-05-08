//! Session and page lifecycle: `vs_session_open`, `vs_session_close`,
//! `vs_open`, `vs_close`.

use vs_protocol::StateToken;

use super::audit::AuditCtx;
use super::responses::{CloseResponse, OpenResponse, SessionCloseResponse, SessionOpenResponse};
use super::{short_id, Daemon, SessionState};
use crate::error::{DaemonError, Result};
use crate::page_state::PageState;
use crate::{redact, tokens};

impl Daemon {
    /// Open a fresh session. Returns the new id and a synthetic empty
    /// session-scoped state token (`0000…`); page-scoped tokens come
    /// from `vs_open`.
    pub fn session_open(&self, policy: Option<&str>) -> Result<SessionOpenResponse> {
        let id = format!("s_{}", short_id());
        let policy_args: Vec<String> = policy.into_iter().map(str::to_string).collect();
        let ctx = AuditCtx::new("vs_session_open", &id).with_args(
            redact::redact_args(
                &[],
                policy
                    .map_or(vec![], |p| vec![("policy".into(), Some(p.into()))])
                    .as_slice(),
            ),
            tokens::args_hash("vs_session_open", &policy_args),
        );
        self.audit_call(ctx, |ctx| {
            ctx.after_token = Some(StateToken::ZERO);
            let mut store = self.inner.store.lock().expect("poisoned");
            let session = store.create_session(&id, policy)?;
            drop(store);
            self.inner
                .sessions
                .lock()
                .expect("poisoned")
                .insert(id.clone(), SessionState::new());
            Ok(SessionOpenResponse {
                session_id: session.id,
                token: StateToken::ZERO,
            })
        })
    }

    pub fn session_close(&self, session_id: &str) -> Result<SessionCloseResponse> {
        let ctx = AuditCtx::new("vs_session_close", session_id)
            .with_args(String::new(), tokens::args_hash("vs_session_close", &[]));
        self.audit_call(ctx, |_ctx| {
            let page_handles: Vec<_> = {
                let mut sessions = self.inner.sessions.lock().expect("poisoned");
                let s = sessions
                    .remove(session_id)
                    .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?;
                s.pages.into_values().map(|p| p.engine_handle).collect()
            };
            for h in page_handles {
                let _ = self.inner.engine.close(h);
            }
            let mut store = self.inner.store.lock().expect("poisoned");
            store.close_session(session_id)?;
            for page in store.list_pages(session_id)? {
                if page.closed_at.is_none() {
                    let _ = store.close_page(&page.id);
                }
            }
            Ok(SessionCloseResponse)
        })
    }

    pub fn open(&self, session_id: &str, url: &str) -> Result<OpenResponse> {
        let url_owned = url.to_string();
        let ctx = AuditCtx::new("vs_open", session_id).with_args(
            url_owned.clone(),
            tokens::args_hash("vs_open", std::slice::from_ref(&url_owned)),
        );
        self.audit_call(ctx, |ctx| {
            self.require_session(session_id)?;

            let engine_handle = self.inner.engine.open(url)?;
            let page_id = format!("p_{}", short_id());
            ctx.page_id = Some(page_id.clone());

            let mut store = self.inner.store.lock().expect("poisoned");
            store.create_page(&page_id, session_id, &url_owned)?;
            drop(store);

            let tree = self.inner.engine.snapshot(engine_handle)?;
            let token = tokens::compute(&tree, &url_owned, &page_id);
            ctx.after_token = Some(token);

            let mut page = PageState::new(page_id.clone(), url_owned.clone(), engine_handle);
            page.last_tree = Some(tree.clone());
            page.last_token = Some(token);
            page.force_full = true;
            for n in &tree {
                page.seen_refs.insert(n.r);
            }

            let mut store = self.inner.store.lock().expect("poisoned");
            store.update_page_token(&page_id, &token.to_string(), "engine", None)?;
            drop(store);

            self.inner
                .sessions
                .lock()
                .expect("poisoned")
                .get_mut(session_id)
                .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?
                .pages
                .insert(page_id.clone(), page);

            Ok(OpenResponse {
                page_id,
                token,
                warnings: Vec::new(),
            })
        })
    }

    pub fn close(&self, session_id: &str, page_id: &str) -> Result<CloseResponse> {
        let ctx = AuditCtx::new("vs_close", session_id)
            .with_page(page_id)
            .with_args(String::new(), tokens::args_hash("vs_close", &[]));
        self.audit_call(ctx, |_ctx| {
            let engine_handle = {
                let mut sessions = self.inner.sessions.lock().expect("poisoned");
                let session = sessions
                    .get_mut(session_id)
                    .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?;
                let page = session
                    .pages
                    .remove(page_id)
                    .ok_or_else(|| DaemonError::UnknownPage(page_id.to_string()))?;
                page.engine_handle
            };
            let _ = self.inner.engine.close(engine_handle);
            let mut store = self.inner.store.lock().expect("poisoned");
            let _ = store.close_page(page_id);
            Ok(CloseResponse)
        })
    }
}
