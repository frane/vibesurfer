//! The [`Store`]: a SQLite-backed handle that owns a single connection.
//!
//! Per-table CRUD lives in submodules ([`sessions`], [`pages`],
//! [`refs`], [`marks`], [`annotations`], [`actions`], [`auth_blobs`],
//! [`skill_cache`]) — each contributes one `impl Store` block.

mod actions;
mod annotations;
mod auth_blobs;
mod marks;
mod pages;
mod refs;
mod sessions;
mod skill_cache;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

use crate::error::Result;
use crate::migrate;

/// The default idempotency window: a `vs_act` repeat is treated as a
/// hit only if the prior call started within this many seconds.
pub const IDEMPOTENCY_TTL_SECS: i64 = 30;

/// SQLite-backed durable state for vibesurfer.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open the store at `path`, applying any not-yet-applied
    /// migrations and configuring WAL mode and foreign keys.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())?;
        configure(&conn)?;
        migrate::apply(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory store (tests only — dropped when the handle
    /// goes out of scope).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        configure(&conn)?;
        migrate::apply(&conn)?;
        Ok(Self { conn })
    }

    /// Open `path` read-only, *without* running migrations. Use for
    /// `vs_log` queries that should never block on a write lock.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(Self { conn })
    }

    /// Borrow the underlying connection. Per-table submodules use this
    /// instead of `self.conn` to keep the field private.
    #[must_use]
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

fn configure(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Current Unix time in seconds, saturating at `i64::MAX` for the
/// year-292277026596 case.
#[must_use]
pub fn epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
