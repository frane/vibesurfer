//! Page inspection reads behind `vs_inspect`: console, network,
//! requests, eval, storage, cookie events, scripts, responses, DOM
//! and performance.

use super::audit::AuditCtx;
use super::{require_capability, Daemon};
use crate::error::Result;

impl Daemon {
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
        frame: Option<&str>,
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
            let r = match frame {
                Some(f) => self.inner.engine.eval_js_in_frame(handle, f, expr)?,
                None => self.inner.engine.eval_js(handle, expr)?,
            };
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

    pub fn inspect_response(
        &self,
        session_id: &str,
        page_id: &str,
    ) -> Result<Option<vs_engine_webkit::inspector::MainResponse>> {
        let ctx = AuditCtx::new("vs_inspect", session_id)
            .with_page(page_id)
            .with_args(
                "response".into(),
                crate::tokens::args_hash("vs_inspect", &["response".into()]),
            );
        self.audit_call(ctx, |ctx| {
            self.require_session(session_id)?;
            let handle = self.engine_handle_for(session_id, page_id)?;
            let resp = self.inner.engine.main_response(handle)?;
            ctx.after_token = Some(self.current_token(session_id, page_id)?);
            Ok(resp)
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
}
