//! Domain types — owned, plain Rust mirrors of the SQLite tables.
//!
//! Conversions to/from `rusqlite::Row` live alongside each type so the
//! query bodies elsewhere in the crate stay short.

use std::fmt;
use std::str::FromStr;

use rusqlite::{Row, ToSql};

use crate::error::{Result, StoreError};

/// Lifecycle of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Open,
    Closed,
}

impl SessionStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SessionStatus {
    type Err = StoreError;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            _ => Err(StoreError::Invalid("session.status")),
        }
    }
}

impl ToSql for SessionStatus {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.as_str()))
    }
}

/// Row of `sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub created_at: i64,
    pub policy_id: Option<String>,
    pub status: SessionStatus,
    pub closed_at: Option<i64>,
}

impl Session {
    pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let status_str: String = row.get("status")?;
        Ok(Self {
            id: row.get("id")?,
            created_at: row.get("created_at")?,
            policy_id: row.get("policy_id")?,
            status: status_str.parse().map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    "session.status".into(),
                )
            })?,
            closed_at: row.get("closed_at")?,
        })
    }
}

/// Row of `pages`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub id: String,
    pub session_id: String,
    pub url: String,
    pub title: Option<String>,
    pub last_token: Option<String>,
    pub last_dom_hash: Option<String>,
    pub last_seen_at: i64,
    pub closed_at: Option<i64>,
}

impl Page {
    pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            session_id: row.get("session_id")?,
            url: row.get("url")?,
            title: row.get("title")?,
            last_token: row.get("last_token")?,
            last_dom_hash: row.get("last_dom_hash")?,
            last_seen_at: row.get("last_seen_at")?,
            closed_at: row.get("closed_at")?,
        })
    }
}

/// Row of `refs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRef {
    pub session_id: String,
    pub page_id: String,
    pub r: u32,
    pub dom_path: String,
    pub role: String,
    pub content_hash: String,
    pub created_at: i64,
    pub retired_at: Option<i64>,
}

impl StoredRef {
    pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let r_i64: i64 = row.get("ref")?;
        Ok(Self {
            session_id: row.get("session_id")?,
            page_id: row.get("page_id")?,
            r: u32::try_from(r_i64).map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Integer,
                    "ref out of u32 range".into(),
                )
            })?,
            dom_path: row.get("dom_path")?,
            role: row.get("role")?,
            content_hash: row.get("content_hash")?,
            created_at: row.get("created_at")?,
            retired_at: row.get("retired_at")?,
        })
    }
}

/// Row of `marks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub id: String,
    pub session_id: String,
    pub page_id: String,
    pub name: String,
    pub dom_path: String,
    pub role: Option<String>,
    pub content_excerpt: Option<String>,
    pub created_at: i64,
}

impl Mark {
    pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            session_id: row.get("session_id")?,
            page_id: row.get("page_id")?,
            name: row.get("name")?,
            dom_path: row.get("dom_path")?,
            role: row.get("role")?,
            content_excerpt: row.get("content_excerpt")?,
            created_at: row.get("created_at")?,
        })
    }
}

/// What an annotation targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnotationTarget {
    Ref(u32),
    Mark(String),
    Page,
}

impl AnnotationTarget {
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Ref(_) => "ref",
            Self::Mark(_) => "mark",
            Self::Page => "page",
        }
    }

    #[must_use]
    pub fn id(&self) -> String {
        match self {
            Self::Ref(r) => r.to_string(),
            Self::Mark(name) => name.clone(),
            Self::Page => String::new(),
        }
    }

    pub(crate) fn parse(kind: &str, id: &str) -> Result<Self> {
        match kind {
            "ref" => {
                let r: u32 = id
                    .parse()
                    .map_err(|_| StoreError::Invalid("annotation.target_id (ref)"))?;
                Ok(Self::Ref(r))
            }
            "mark" => Ok(Self::Mark(id.to_string())),
            "page" => Ok(Self::Page),
            _ => Err(StoreError::Invalid("annotation.target_kind")),
        }
    }
}

/// Row of `annotations`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub id: String,
    pub target: AnnotationTarget,
    pub key: String,
    pub value: Option<String>,
    pub created_at: i64,
}

impl Annotation {
    pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let kind: String = row.get("target_kind")?;
        let id: String = row.get("target_id")?;
        let target = AnnotationTarget::parse(&kind, &id).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                "annotation.target".into(),
            )
        })?;
        Ok(Self {
            id: row.get("id")?,
            target,
            key: row.get("key")?,
            value: row.get("value")?,
            created_at: row.get("created_at")?,
        })
    }
}

/// Row of `actions`. Auditable record of one primitive call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub id: i64,
    pub session_id: String,
    pub page_id: Option<String>,
    pub primitive: String,
    pub args_redacted: String,
    pub args_hash: String,
    pub before_token: Option<String>,
    pub after_token: Option<String>,
    pub idempotency_hit: bool,
    pub result_summary: Option<String>,
    pub latency_ms: i64,
    pub group_label: Option<String>,
    pub started_at: i64,
    pub finished_at: i64,
    pub error_code: Option<String>,
}

impl Action {
    pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let idem: i64 = row.get("idempotency_hit")?;
        Ok(Self {
            id: row.get("id")?,
            session_id: row.get("session_id")?,
            page_id: row.get("page_id")?,
            primitive: row.get("primitive")?,
            args_redacted: row.get("args_redacted")?,
            args_hash: row.get("args_hash")?,
            before_token: row.get("before_token")?,
            after_token: row.get("after_token")?,
            idempotency_hit: idem != 0,
            result_summary: row.get("result_summary")?,
            latency_ms: row.get("latency_ms")?,
            group_label: row.get("group_label")?,
            started_at: row.get("started_at")?,
            finished_at: row.get("finished_at")?,
            error_code: row.get("error_code")?,
        })
    }
}

/// What [`Store::record_action`](crate::Store::record_action) takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionInsert {
    pub session_id: String,
    pub page_id: Option<String>,
    pub primitive: String,
    pub args_redacted: String,
    pub args_hash: String,
    pub before_token: Option<String>,
    pub after_token: Option<String>,
    pub idempotency_hit: bool,
    pub result_summary: Option<String>,
    pub latency_ms: i64,
    pub group_label: Option<String>,
    pub started_at: i64,
    pub finished_at: i64,
    pub error_code: Option<String>,
}

/// Filter for [`Store::list_actions`](crate::Store::list_actions).
#[derive(Debug, Clone, Default)]
pub struct ActionFilter {
    pub session_id: Option<String>,
    pub page_id: Option<String>,
    pub group_label: Option<String>,
    pub since_started_at: Option<i64>,
    pub limit: Option<i64>,
}

/// Metadata for a stored auth blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthBlobMeta {
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

impl AuthBlobMeta {
    pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            name: row.get("name")?,
            created_at: row.get("created_at")?,
            last_used_at: row.get("last_used_at")?,
        })
    }
}

/// Row of `skill_cache`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub version: String,
    pub sha: String,
    pub manifest: String,
    pub last_used_at: Option<i64>,
}

impl SkillEntry {
    pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            name: row.get("name")?,
            version: row.get("version")?,
            sha: row.get("sha")?,
            manifest: row.get("manifest")?,
            last_used_at: row.get("last_used_at")?,
        })
    }
}
