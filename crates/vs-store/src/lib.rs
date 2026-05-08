//! SQLite-backed durable state for vibesurfer.
//!
//! The store owns one SQLite database file at `~/.vibesurfer/state.db`
//! (or in-memory for tests). It is the **source of truth** for every
//! piece of cross-restart state: sessions, pages, refs, marks,
//! annotations, the audit log, encrypted auth blobs, and the skill
//! cache. The daemon's in-memory caches are derivative — on restart
//! they are rebuilt from this crate.
//!
//! # Layout
//!
//! - [`Store`] — the main handle. CRUD methods for every table.
//! - [`types`] — owned domain types mirroring the SQL schema.
//! - [`auth`] — AES-256-GCM helpers and master-key resolution
//!   (OS keyring → `~/.vibesurfer/key`).
//! - [`error::StoreError`] — every fallible function returns this.
//! - [`migrate`] — the migration runner; called automatically by
//!   [`Store::open`].

#![forbid(unsafe_code)]

pub mod auth;
pub mod error;
pub mod migrate;
mod store;
pub mod types;

pub use auth::{decrypt, encrypt, EncryptedBlob, MasterKey};
pub use error::{Result, StoreError};
pub use store::{epoch_secs, Store, IDEMPOTENCY_TTL_SECS};
pub use types::{
    Action, ActionFilter, ActionInsert, Annotation, AnnotationTarget, AuthBlobMeta, Mark, Page,
    Session, SessionStatus, SkillEntry, StoredRef,
};

/// Returns the crate version (matches the workspace version).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn version_matches_cargo_pkg_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}
