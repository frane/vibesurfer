//! Native coordinate input (`vs_move_to`, `vs_click_at`, `vs_hover_at`,
//! `vs_wheel`, `vs_drag`) and trusted typing (`vs_type`).

use super::audit::AuditCtx;
use super::Daemon;
use crate::error::{DaemonError, Result};

impl Daemon {
    pub fn cursor_op(
        &self,
        session_id: &str,
        page_id: &str,
        op: vs_engine_webkit::engine::CursorOp,
        mode: vs_engine_webkit::engine::InputMode,
    ) -> Result<(vs_protocol::StateToken, crate::page_state::ViewForm)> {
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
            let tree = self.inner.engine.snapshot(handle)?;
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
            ctx.after_token = Some(token);
            let mut store = self.inner.store.lock().expect("poisoned");
            store.update_page_token(page_id, &token.to_string(), "engine", None)?;
            drop(store);
            Ok((token, form))
        })
    }

    /// Trusted keyboard typing into the focused element (see
    /// [`vs_engine_webkit::Engine::type_text`]). Secret is redacted in
    /// the audit log — the length only.
    pub fn type_text(
        &self,
        session_id: &str,
        page_id: &str,
        text: &str,
        secret: bool,
        mode: vs_engine_webkit::engine::InputMode,
    ) -> Result<vs_protocol::StateToken> {
        let redacted = if secret {
            format!(
                "*** ({} chars) mode={}",
                text.chars().count(),
                mode.as_str()
            )
        } else {
            format!("{} chars mode={}", text.chars().count(), mode.as_str())
        };
        let ctx = AuditCtx::new("vs_type", session_id)
            .with_page(page_id)
            .with_args(
                redacted.clone(),
                crate::tokens::args_hash("vs_type", &[redacted]),
            );
        self.audit_call(ctx, |ctx| {
            self.require_session(session_id)?;
            let handle = self.engine_handle_for(session_id, page_id)?;
            self.inner.engine.type_text(handle, text, mode)?;
            let token = self.current_token(session_id, page_id)?;
            ctx.after_token = Some(token);
            Ok(token)
        })
    }
}
