//! Session and page lifecycle: `vs_session_open`, `vs_session_close`,
//! `vs_open`, `vs_close`.

use std::time::Instant;

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
                s.pages
                    .into_values()
                    .filter_map(|p| p.engine_handle)
                    .collect()
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

    /// Close every session, or every session untouched for at least
    /// `idle_for`. Returns the ids closed.
    ///
    /// The single-session close assumes a caller tidying up after
    /// itself, which is exactly what a crashed agent never does. This
    /// is the remedy for what they leave behind, and it is a remedy a
    /// human can aim: `--idle-for=2h` spares whatever is still in use,
    /// where the previous answer — kill the daemon — took live
    /// authenticated pages with it.
    pub fn session_close_sweep(
        &self,
        idle_for: Option<std::time::Duration>,
    ) -> Result<Vec<String>> {
        let now = Instant::now();
        let targets: Vec<String> = {
            let sessions = self.inner.sessions.lock().expect("poisoned");
            sessions
                .iter()
                .filter(|(_, s)| {
                    idle_for.is_none_or(|d| {
                        let newest = s
                            .pages
                            .values()
                            .map(|p| p.last_touched)
                            .fold(s.last_touched, Instant::max);
                        now.saturating_duration_since(newest) >= d
                    })
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        let mut closed = Vec::new();
        for id in targets {
            match self.session_close(&id) {
                Ok(_) => closed.push(id),
                // One wedged session must not stop the sweep; the
                // caller is reaching for this because things are
                // already in a bad state.
                Err(e) => tracing::warn!(session = %id, error = %e, "sweep close failed"),
            }
        }
        Ok(closed)
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

    /// Navigate an existing page to `url` in place (`vs_goto`). Reuses
    /// the page's web view instead of creating a new page, then
    /// re-baselines: the document is replaced, so all refs are fresh
    /// and the next view is a full tree.
    pub fn navigate(&self, session_id: &str, page_id: &str, url: &str) -> Result<OpenResponse> {
        let url_owned = url.to_string();
        let ctx = AuditCtx::new("vs_goto", session_id)
            .with_page(page_id)
            .with_args(
                url_owned.clone(),
                tokens::args_hash("vs_goto", std::slice::from_ref(&url_owned)),
            );
        self.audit_call(ctx, |ctx| {
            let engine_handle = self.engine_handle_for(session_id, page_id)?;
            self.inner.engine.navigate(engine_handle, url)?;

            let tree = self.inner.engine.snapshot(engine_handle)?;
            let token = tokens::compute(&tree, &url_owned, page_id);
            ctx.after_token = Some(token);

            {
                let mut sessions = self.inner.sessions.lock().expect("poisoned");
                let page = sessions
                    .get_mut(session_id)
                    .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?
                    .pages
                    .get_mut(page_id)
                    .ok_or_else(|| DaemonError::UnknownPage(page_id.to_string()))?;
                page.url.clone_from(&url_owned);
                page.last_tree = Some(tree.clone());
                page.last_token = Some(token);
                page.force_full = true;
                page.seen_refs.clear();
                for n in &tree {
                    page.seen_refs.insert(n.r);
                }
            }

            let mut store = self.inner.store.lock().expect("poisoned");
            store.update_page_url(page_id, &url_owned)?;
            store.update_page_token(page_id, &token.to_string(), "engine", None)?;
            drop(store);

            Ok(OpenResponse {
                page_id: page_id.to_string(),
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
            if let Some(h) = engine_handle {
                let _ = self.inner.engine.close(h);
            }
            let mut store = self.inner.store.lock().expect("poisoned");
            let _ = store.close_page(page_id);
            Ok(CloseResponse)
        })
    }

    /// Rebuild in-memory sessions from the store after a daemon
    /// restart. ARCHITECTURE.md's contract is that anything held in
    /// memory is reconstructible from state.db — before this, a
    /// restart silently dropped every open session (an agent parked
    /// mid-flow saw WRONG_SESSION and lost its login). Session and
    /// page ids are preserved; each page reopens at its last URL with
    /// a fresh engine page (cookies come from the engine's persistent
    /// data store, or `vs auth load`). Open sessions with no page
    /// activity within `max_age` are closed in the store instead —
    /// nothing else prunes rows for daemons that died before closing.
    /// Returns (sessions, pages) resurrected.
    #[must_use]
    pub fn resurrect_sessions(&self, max_age: std::time::Duration) -> (usize, usize) {
        let now = vs_store::epoch_secs();
        let cutoff = now.saturating_sub(i64::try_from(max_age.as_secs()).unwrap_or(i64::MAX));
        let rows = {
            let store = self.inner.store.lock().expect("poisoned");
            store.list_sessions().unwrap_or_default()
        };
        let mut n_sessions = 0usize;
        let mut n_pages = 0usize;
        for s in rows {
            if s.status != vs_store::SessionStatus::Open {
                continue;
            }
            let pages = {
                let store = self.inner.store.lock().expect("poisoned");
                store.list_pages(&s.id).unwrap_or_default()
            };
            let live: Vec<_> = pages
                .into_iter()
                .filter(|p| p.closed_at.is_none() && p.last_seen_at >= cutoff)
                .collect();
            if live.is_empty() {
                let mut store = self.inner.store.lock().expect("poisoned");
                let _ = store.close_session(&s.id);
                continue;
            }
            let mut state = SessionState::new();
            for p in live {
                // Dormant: the engine page is created lazily on
                // first use, so startup stays instant and zombie
                // sessions never cost a webview.
                state
                    .pages
                    .insert(p.id.clone(), PageState::dormant(p.id, p.url));
                n_pages += 1;
            }
            self.inner
                .sessions
                .lock()
                .expect("poisoned")
                .insert(s.id.clone(), state);
            n_sessions += 1;
        }
        (n_sessions, n_pages)
    }
}
