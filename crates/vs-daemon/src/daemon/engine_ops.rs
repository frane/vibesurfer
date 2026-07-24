//! Engine-backed primitives: `vs_skill`, `vs_capture`, `vs_viewport`,
//! `vs_layout`, `vs_auth`.

use vs_engine_webkit::{AuthBlob, CaptureScope, Viewport};
use vs_protocol::{Ref, StateToken, Warning, WarningCode};

use super::audit::AuditCtx;
use super::responses::{
    AuthClearResponse, AuthListResponse, AuthLoadResponse, AuthSaveResponse, CaptureResponse,
    LayoutResponse, SkillListResponse, SkillShowResponse, ViewportResponse,
};
use super::Daemon;
use crate::error::{DaemonError, Result};
use crate::tokens;

impl Daemon {
    /// List skills available in `skills_dir`. Execution dispatch lands
    /// in M6.
    pub fn skill_list(&self, session_id: &str) -> Result<SkillListResponse> {
        let ctx = AuditCtx::new("vs_skill", session_id).with_args(
            "list".into(),
            tokens::args_hash("vs_skill", &["list".into()]),
        );
        self.audit_call(ctx, |_ctx| {
            self.require_session(session_id)?;
            let mut names = Vec::new();
            let entries = match std::fs::read_dir(&self.inner.skills_dir) {
                Ok(it) => it,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(SkillListResponse { names });
                }
                Err(e) => return Err(DaemonError::Io(e)),
            };
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    if let Some(n) = entry.file_name().to_str() {
                        names.push(n.to_string());
                    }
                }
            }
            names.sort();
            Ok(SkillListResponse { names })
        })
    }

    /// Show the SKILL.md for a named skill.
    pub fn skill_show(&self, session_id: &str, name: &str) -> Result<SkillShowResponse> {
        let ctx = AuditCtx::new("vs_skill", session_id).with_args(
            format!("show {name}"),
            tokens::args_hash("vs_skill", &["show".into(), name.to_string()]),
        );
        self.audit_call(ctx, |_ctx| {
            self.require_session(session_id)?;
            let path = self.inner.skills_dir.join(name).join("SKILL.md");
            let body = std::fs::read_to_string(&path).map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => DaemonError::BadRequest(format!(
                    "skill not found: {name} (looked in {})",
                    path.display()
                )),
                _ => DaemonError::Io(e),
            })?;
            Ok(SkillShowResponse { body })
        })
    }

    /// Take a screenshot. Returns the on-disk path; bytes are not
    /// inlined per `docs/PROTOCOL.md`.
    pub fn capture(
        &self,
        session_id: &str,
        page_id: &str,
        scope: CaptureScope,
    ) -> Result<CaptureResponse> {
        let scope_label = match &scope {
            CaptureScope::Viewport => "viewport".to_string(),
            CaptureScope::Element(r) => format!("ref:{r}"),
            CaptureScope::FullPage => "full-page".to_string(),
        };
        let ctx = AuditCtx::new("vs_capture", session_id)
            .with_page(page_id)
            .with_args(
                scope_label.clone(),
                tokens::args_hash("vs_capture", &[scope_label]),
            );
        self.audit_call(ctx, |ctx| {
            let engine_handle = self.engine_handle_for(session_id, page_id)?;
            std::fs::create_dir_all(&self.inner.captures_dir).map_err(DaemonError::Io)?;
            let path = self.inner.engine.capture(engine_handle, scope)?;
            // Auto-cap: keep the captures dir bounded so PNGs don't grow
            // without limit. The file just written is the newest, so the
            // newest-N + max-age policy never deletes it.
            let stats = crate::captures::prune(
                &self.inner.captures_dir,
                &crate::captures::RetentionPolicy::default_cap(),
                std::time::SystemTime::now(),
            );
            if stats.deleted > 0 {
                eprintln!(
                    "vs: auto-pruned {} old capture(s), {} KiB freed ({} kept) in {}",
                    stats.deleted,
                    stats.freed_bytes / 1024,
                    stats.kept,
                    self.inner.captures_dir.display(),
                );
            }
            let token = self
                .current_token(session_id, page_id)
                .unwrap_or(StateToken::ZERO);
            ctx.after_token = Some(token);
            ctx.result_summary = Some(path.display().to_string());
            Ok(CaptureResponse { path, token })
        })
    }

    /// Set the page viewport. Triggers a fresh-full re-baseline on the
    /// next `vs_view` (the protocol's `? viewport_changed` warning is
    /// the agent-visible cue).
    pub fn viewport(
        &self,
        session_id: &str,
        page_id: &str,
        viewport: Viewport,
    ) -> Result<ViewportResponse> {
        let arg = format!("{}x{}@{}dpr", viewport.width, viewport.height, viewport.dpr);
        let ctx = AuditCtx::new("vs_viewport", session_id)
            .with_page(page_id)
            .with_args(arg.clone(), tokens::args_hash("vs_viewport", &[arg]));
        self.audit_call(ctx, |ctx| {
            let engine_handle = self.engine_handle_for(session_id, page_id)?;
            self.inner.engine.set_viewport(engine_handle, viewport)?;
            {
                let mut sessions = self.inner.sessions.lock().expect("poisoned");
                let page = sessions
                    .get_mut(session_id)
                    .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?
                    .pages
                    .get_mut(page_id)
                    .ok_or_else(|| DaemonError::UnknownPage(page_id.to_string()))?;
                page.invalidate_baseline();
            }
            let tree = self.inner.engine.snapshot(engine_handle)?;
            let (token, _form) = {
                let mut sessions = self.inner.sessions.lock().expect("poisoned");
                let page = sessions
                    .get_mut(session_id)
                    .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?
                    .pages
                    .get_mut(page_id)
                    .ok_or_else(|| DaemonError::UnknownPage(page_id.to_string()))?;
                let result = page.apply_snapshot(tree);
                // Leave force_full=true so the *next* vs_view emits a
                // fresh full tree per docs/PROTOCOL.md.
                page.invalidate_baseline();
                result
            };
            ctx.after_token = Some(token);

            let mut store = self.inner.store.lock().expect("poisoned");
            store.update_page_token(page_id, &token.to_string(), "engine", None)?;
            drop(store);
            Ok(ViewportResponse {
                token,
                warnings: vec![Warning::with_args(
                    WarningCode::ViewportChanged,
                    vec![format!("{}x{}", viewport.width, viewport.height)],
                )],
            })
        })
    }

    pub fn layout(
        &self,
        session_id: &str,
        page_id: &str,
        refs: Vec<Ref>,
    ) -> Result<LayoutResponse> {
        let arg_strs: Vec<String> = refs.iter().map(Ref::to_string).collect();
        let ctx = AuditCtx::new("vs_layout", session_id)
            .with_page(page_id)
            .with_args(
                arg_strs.join(" "),
                tokens::args_hash("vs_layout", &arg_strs),
            );
        self.audit_call(ctx, |ctx| {
            let engine_handle = self.engine_handle_for(session_id, page_id)?;
            let boxes = self.inner.engine.layout(engine_handle, refs)?;
            let token = self
                .current_token(session_id, page_id)
                .unwrap_or(StateToken::ZERO);
            ctx.after_token = Some(token);
            Ok(LayoutResponse { boxes, token })
        })
    }

    pub fn auth_save(
        &self,
        session_id: &str,
        page_id: &str,
        name: &str,
    ) -> Result<AuthSaveResponse> {
        let ctx = AuditCtx::new("vs_auth", session_id)
            .with_page(page_id)
            .with_args(
                format!("save {name}"),
                tokens::args_hash("vs_auth", &["save".into(), name.to_string()]),
            );
        self.audit_call(ctx, |_ctx| {
            let key = self.require_master_key()?;
            let engine_handle = self.engine_handle_for(session_id, page_id)?;
            let blob = self.inner.engine.save_auth(engine_handle)?;
            let mut store = self.inner.store.lock().expect("poisoned");
            store.save_auth(name, key, &blob.bytes)?;
            Ok(AuthSaveResponse {
                name: name.to_string(),
            })
        })
    }

    /// Import an externally-captured session (`vs auth import`): store a
    /// human-supplied cookies+storage blob under `name` so a later
    /// `vs auth load` injects it. This is the passkey fallback — when an
    /// origin requires a WebAuthn/passkey login the headless engine
    /// cannot drive, the human logs in in their own browser, exports the
    /// session (cookies + local/session storage as a v2 auth-blob JSON),
    /// and imports it here. No page is touched; it is a pure store write.
    pub fn auth_import(
        &self,
        session_id: &str,
        name: &str,
        blob_json: &[u8],
    ) -> Result<AuthSaveResponse> {
        let ctx = AuditCtx::new("vs_auth", session_id).with_args(
            format!("import {name}"),
            tokens::args_hash("vs_auth", &["import".into(), name.to_string()]),
        );
        self.audit_call(ctx, |_ctx| {
            self.require_session(session_id)?;
            let key = self.require_master_key()?;
            let bytes = vs_engine_webkit::normalize_auth_blob(blob_json)
                .map_err(|e| DaemonError::BadRequest(format!("invalid auth blob: {e}")))?;
            let mut store = self.inner.store.lock().expect("poisoned");
            store.save_auth(name, key, &bytes)?;
            Ok(AuthSaveResponse {
                name: name.to_string(),
            })
        })
    }

    pub fn auth_load(
        &self,
        session_id: &str,
        page_id: &str,
        name: &str,
    ) -> Result<AuthLoadResponse> {
        let ctx = AuditCtx::new("vs_auth", session_id)
            .with_page(page_id)
            .with_args(
                format!("load {name}"),
                tokens::args_hash("vs_auth", &["load".into(), name.to_string()]),
            );
        self.audit_call(ctx, |ctx| {
            let key = self.require_master_key()?;
            let engine_handle = self.engine_handle_for(session_id, page_id)?;
            let bytes = {
                let mut store = self.inner.store.lock().expect("poisoned");
                store.load_auth(name, key)?
            };
            self.inner
                .engine
                .load_auth(engine_handle, AuthBlob { bytes })?;
            {
                let mut sessions = self.inner.sessions.lock().expect("poisoned");
                let page = sessions
                    .get_mut(session_id)
                    .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?
                    .pages
                    .get_mut(page_id)
                    .ok_or_else(|| DaemonError::UnknownPage(page_id.to_string()))?;
                page.invalidate_baseline();
            }
            let token = self
                .current_token(session_id, page_id)
                .unwrap_or(StateToken::ZERO);
            ctx.after_token = Some(token);
            Ok(AuthLoadResponse {
                token,
                warnings: vec![Warning::with_args(
                    WarningCode::AuthLoaded,
                    vec![name.to_string()],
                )],
            })
        })
    }

    pub fn auth_list(&self, session_id: &str) -> Result<AuthListResponse> {
        let ctx = AuditCtx::new("vs_auth", session_id).with_args(
            "list".into(),
            tokens::args_hash("vs_auth", &["list".into()]),
        );
        self.audit_call(ctx, |_ctx| {
            self.require_session(session_id)?;
            let store = self.inner.store.lock().expect("poisoned");
            let entries = store.list_auth()?;
            Ok(AuthListResponse {
                names: entries.into_iter().map(|m| m.name).collect(),
            })
        })
    }

    pub fn auth_clear(&self, session_id: &str, name: &str) -> Result<AuthClearResponse> {
        let ctx = AuditCtx::new("vs_auth", session_id).with_args(
            format!("clear {name}"),
            tokens::args_hash("vs_auth", &["clear".into(), name.to_string()]),
        );
        self.audit_call(ctx, |_ctx| {
            self.require_session(session_id)?;
            let mut store = self.inner.store.lock().expect("poisoned");
            store.delete_auth(name)?;
            Ok(AuthClearResponse)
        })
    }
}
