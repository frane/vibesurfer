//! Human-in-the-loop: the pending-input queue, browser entry forms,
//! the live watch view and its frames.

use vs_engine_webkit::{ActTarget as EngineActTarget, Action as EngineAction};
use vs_protocol::StateToken;

use super::audit::AuditCtx;
use super::{pending, webentry, ActCall, Daemon};
use crate::error::{DaemonError, Result};
use std::sync::Arc;

impl Daemon {
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
            mode: vs_engine_webkit::engine::InputMode::Careful,
            group_label: group,
        };
        let resp = self.act(call)?;
        Ok(resp.token)
    }

    /// Mint an unscoped browser entry URL for the pending queue,
    /// starting the loopback web surface on first use. The page at
    /// the URL renders every pending entry as a form; submitting
    /// fulfills them. Form links are scoped instead
    /// ([`webentry::WebEntry::mint_form`]).
    pub fn web_entry_url(&self) -> Result<String> {
        Ok(self.web_surface()?.mint())
    }

    /// The lazily-started loopback surface shared by entry forms and
    /// live views.
    fn web_surface(&self) -> Result<Arc<webentry::WebEntry>> {
        let mut guard = self.inner.webentry.lock().unwrap();
        if let Some(s) = guard.as_ref() {
            return Ok(s.clone());
        }
        // Weak, or the surface's closure would keep `Inner` alive in a
        // cycle (Inner -> WebEntry -> closure -> Inner).
        let weak = Arc::downgrade(&self.inner);
        let frame: webentry::FrameFn = Arc::new(move |page: &str| {
            let inner = weak.upgrade().ok_or_else(|| "daemon gone".to_string())?;
            Daemon { inner }
                .live_frame(page, 1440)
                .map_err(|e| e.to_string())
        });
        let s = webentry::WebEntry::start(self.inner.pending.clone(), frame)?;
        *guard = Some(s.clone());
        Ok(s)
    }

    /// Mint a live-view URL for `page`, addressed like every other
    /// page op (so a page in another session errors `WrongSession`).
    /// One audit row per mint; frames are presentation and are not
    /// audited. The URL's nonce is a capability — it is returned to
    /// the caller and never logged.
    pub fn watch_url(&self, session_id: &str, page_id: &str) -> Result<String> {
        let ctx = AuditCtx::new("vs_watch", session_id)
            .with_page(page_id)
            .with_args(
                String::new(),
                crate::tokens::args_hash("vs_watch", &[page_id.to_string()]),
            );
        self.audit_call(ctx, |ctx| {
            // Validates session + page (incl. WrongSession routing).
            let _ = self.engine_handle_for(session_id, page_id)?;
            let url = self.web_surface()?.mint_live(page_id);
            ctx.result_summary = Some("live-url minted".into());
            Ok(url)
        })
    }

    /// One transient viewport PNG of `page_id`, session resolved by
    /// page id — no session addressing, no audit row, and the file is
    /// NOT retained by the captures-dir policy: the caller must
    /// delete it after reading. Serves `/live/<nonce>/frame` and the
    /// wire op `vs_frame` (MCP panel frames and action thumbnails).
    pub fn live_frame_path(&self, page_id: &str, max_width: u32) -> Result<std::path::PathBuf> {
        let session_id = {
            let sessions = self.inner.sessions.lock().expect("poisoned");
            sessions
                .iter()
                .find_map(|(sid, s)| s.pages.contains_key(page_id).then(|| sid.clone()))
                .ok_or_else(|| DaemonError::UnknownPage(page_id.to_string()))?
        };
        let handle = self.engine_handle_for(&session_id, page_id)?;
        std::fs::create_dir_all(&self.inner.captures_dir).map_err(DaemonError::Io)?;
        Ok(self.inner.engine.capture_live(handle, max_width)?)
    }

    /// [`Self::live_frame_path`], read and deleted: PNG bytes.
    pub(crate) fn live_frame(&self, page_id: &str, max_width: u32) -> Result<Vec<u8>> {
        let path = self.live_frame_path(page_id, max_width)?;
        let bytes = std::fs::read(&path).map_err(DaemonError::Io)?;
        let _ = std::fs::remove_file(&path);
        Ok(bytes)
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
        // An identical ask on the same page supersedes the earlier
        // one: its waiter gave up, nobody else holds its id, and
        // leaving it queued put a second copy of the same fields in
        // front of the human with nothing to tell them apart.
        let refs: Vec<u32> = fields.iter().map(|(r, _, _)| r.0).collect();
        self.inner.pending.supersede(page_id, &refs);
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
        // Scoped to this form: the previous form's entries stay in
        // the queue until they are collected or reaped, and an
        // unscoped link rendered those too — every re-enqueue grew
        // the page by another copy of the fields.
        let url = self.web_surface()?.mint_form(&form_id);
        Ok((form_id, url))
    }

    /// Park until every field of `form_id` is fulfilled (browser
    /// submit, or `vs pending fulfill` per entry), then fill each ref
    /// in declaration order and return the final state token. The
    /// first fill validates against the token captured at enqueue;
    /// later fills chain the token forward.
    ///
    /// The three failure modes are reported separately, and a waiter
    /// that merely ran out of budget gets its own wire code
    /// ([`DaemonError::PromptFormPending`] -> `TIMEOUT`). Lumping them
    /// into one "cancelled, timed out, or unknown" string cost more
    /// than it saved: an agent whose wait ran out of budget read that
    /// as a dead form, told the human the link was burned, and
    /// abandoned a form that was still live and still waiting for them
    /// to finish typing.
    pub fn prompt_form_wait(
        &self,
        session_id: &str,
        form_id: &str,
        timeout: std::time::Duration,
    ) -> Result<StateToken> {
        let values = match self.inner.pending.wait_form(form_id, timeout) {
            pending::FormWait::Ready(v) => v,
            pending::FormWait::StillPending => {
                return Err(DaemonError::PromptFormPending {
                    form: form_id.to_string(),
                    budget_ms: timeout.as_millis(),
                })
            }
            pending::FormWait::Cancelled => {
                return Err(DaemonError::BadRequest(format!(
                    "vs_prompt_form: form {form_id} was cancelled"
                )))
            }
            pending::FormWait::Unknown => {
                return Err(DaemonError::BadRequest(format!(
                    "vs_prompt_form: form {form_id} unknown: already collected by an \
                     earlier wait, or expired. Enqueue a new form."
                )))
            }
        };
        // A fill can still fail after the human has typed: the ref may
        // be gone, the page may have navigated, the caller may have
        // addressed the wrong session. `wait_form` has already taken
        // the form out of the queue by now, so without this copy that
        // failure destroyed the values — the human's password was gone
        // and the agent's retry was told the form was unknown, which
        // reads as "you imagined it". Their typing is the one thing
        // here that cannot be recreated, so it goes back on any error
        // and the retry can pick up where this left off.
        let held = values.clone();
        let mut token: Option<StateToken> = None;
        for (entry, value) in values {
            let before_token: StateToken = match token {
                // Subsequent fields chain off the previous fill, which
                // is the write this one is genuinely sequenced after.
                Some(t) => t,
                // First field: read the page's *current* token rather
                // than the one stored when `vs_prompt_form` was called.
                //
                // That stored token was captured before the human had
                // even opened the entry URL. They then take seconds or
                // minutes to type, and any re-render in that window —
                // which a framework-driven login form does constantly,
                // on validation, focus and timers — advances the token.
                // The fill then failed the stale-token check and
                // silently never ran: the pending entry was consumed,
                // the daemon reported the form fulfilled, and the field
                // stayed empty. Secure credential entry into a React or
                // Vue login was impossible, which is the whole reason
                // this primitive exists.
                //
                // The stale-token guard is there to stop an *agent*
                // writing on the strength of a stale read. This is not
                // that: the human authorised this exact value for this
                // exact ref, and the delay is the feature. If the ref
                // itself is gone the fill still fails loudly with
                // NOT_FOUND, which is the honest error.
                None => self
                    .current_token(session_id, &entry.page)
                    .unwrap_or(StateToken::ZERO),
            };
            let resp = match self.act(ActCall {
                session_id: session_id.to_string(),
                page_id: entry.page.clone(),
                target: EngineActTarget::Ref(vs_protocol::Ref(entry.r)),
                action: EngineAction::Fill { value },
                before_token,
                args_hash: crate::tokens::args_hash("vs_act", &["fill".into(), "***".into()]),
                args_redacted: "fill ***".into(),
                mode: vs_engine_webkit::engine::InputMode::Careful,
                group_label: entry.group.clone(),
            }) {
                Ok(resp) => resp,
                Err(e) => {
                    // Hand the values back and report the real error.
                    // Re-filling a field this loop already wrote is
                    // harmless — a fill is idempotent — and it keeps
                    // the retry a plain repeat of the whole form
                    // rather than a partial resume.
                    self.inner.pending.restore(held);
                    return Err(e);
                }
            };
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
