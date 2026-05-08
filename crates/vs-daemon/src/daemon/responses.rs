//! Per-primitive response types and the [`ActCall`] input bundle.
//!
//! Split out of `daemon/mod.rs` so each handler file can `use
//! super::responses::*;` without dragging in the rest of the daemon's
//! internals.

use vs_engine_webkit::{ActTarget as EngineTarget, Action as EngineAction, LayoutBox};
use vs_protocol::{Ref, StateToken, Warning};

use crate::page_state::ViewForm;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOpenResponse {
    pub session_id: String,
    pub token: StateToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCloseResponse;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenResponse {
    pub page_id: String,
    pub token: StateToken,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseResponse;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewResponse {
    pub token: StateToken,
    pub form: ViewForm,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadResponse {
    pub token: StateToken,
    pub body: String,
}

/// Inputs to [`crate::Daemon::act`]. Bundled into a struct so the
/// method signature stays one-arg and clippy's `too_many_arguments`
/// lint is satisfied honestly.
#[derive(Debug, Clone)]
pub struct ActCall {
    pub session_id: String,
    pub page_id: String,
    pub target: EngineTarget,
    pub action: EngineAction,
    pub before_token: StateToken,
    pub args_hash: String,
    pub args_redacted: String,
    pub group_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActResponse {
    pub token: StateToken,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindResponse {
    pub hits: Vec<FindHit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindHit {
    pub page_id: String,
    pub r: Ref,
    pub role: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitResponse {
    pub token: StateToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusResponse {
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractResponse {
    pub token: StateToken,
    pub records: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkResponse {
    pub mark_id: String,
    pub token: StateToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotateResponse {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogResponse {
    pub rows: Vec<vs_store::Action>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillListResponse {
    pub names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillShowResponse {
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureResponse {
    pub path: std::path::PathBuf,
    pub token: StateToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewportResponse {
    pub token: StateToken,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayoutResponse {
    pub boxes: Vec<LayoutBox>,
    pub token: StateToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSaveResponse {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthLoadResponse {
    pub token: StateToken,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthListResponse {
    pub names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthClearResponse;
