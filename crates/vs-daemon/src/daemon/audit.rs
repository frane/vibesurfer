//! [`AuditCtx`] — the audit-row builder threaded through every
//! primitive via [`crate::Daemon::audit_call`]. Fields are mutated
//! by the primitive body as it learns information (page id,
//! post-action token, idempotency hit, error code).

use vs_protocol::StateToken;

#[derive(Debug, Clone)]
pub(crate) struct AuditCtx {
    pub(crate) primitive: &'static str,
    pub(crate) session_id: String,
    pub(crate) page_id: Option<String>,
    pub(crate) args_redacted: String,
    pub(crate) args_hash: String,
    pub(crate) before_token: Option<StateToken>,
    pub(crate) after_token: Option<StateToken>,
    pub(crate) idempotency_hit: bool,
    pub(crate) result_summary: Option<String>,
    pub(crate) group_label: Option<String>,
}

impl AuditCtx {
    pub(crate) fn new(primitive: &'static str, session_id: impl Into<String>) -> Self {
        Self {
            primitive,
            session_id: session_id.into(),
            page_id: None,
            args_redacted: String::new(),
            args_hash: String::new(),
            before_token: None,
            after_token: None,
            idempotency_hit: false,
            result_summary: None,
            group_label: None,
        }
    }

    pub(crate) fn with_args(mut self, redacted: String, hash: String) -> Self {
        self.args_redacted = redacted;
        self.args_hash = hash;
        self
    }

    pub(crate) fn with_page(mut self, page_id: impl Into<String>) -> Self {
        self.page_id = Some(page_id.into());
        self
    }

    pub(crate) fn with_before(mut self, t: StateToken) -> Self {
        self.before_token = Some(t);
        self
    }

    pub(crate) fn with_group(mut self, g: Option<String>) -> Self {
        self.group_label = g;
        self
    }
}
