//! Schema migrations.
//!
//! Migrations are numbered SQL files in `crates/vs-store/migrations/`.
//! On open, [`apply`] runs every migration whose name is not yet recorded
//! in `_migrations`. Migrations are content-addressed by name; never
//! rename or rewrite a published migration — add a new one instead.

use rusqlite::Connection;

use crate::error::Result;

const MIGRATIONS: &[(&str, &str)] = &[(
    "0001_initial",
    include_str!("../migrations/0001_initial.sql"),
)];

/// Apply any not-yet-applied migrations against `conn`. Idempotent.
pub fn apply(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _migrations (
            name        TEXT PRIMARY KEY,
            applied_at  INTEGER NOT NULL
        );",
    )?;

    for (name, sql) in MIGRATIONS {
        let already: i64 = conn.query_row(
            "SELECT COUNT(*) FROM _migrations WHERE name = ?1",
            [name],
            |row| row.get(0),
        )?;
        if already > 0 {
            continue;
        }
        // Run the migration body and the bookkeeping insert as one
        // transaction so a half-applied migration doesn't poison the
        // store.
        conn.execute_batch("BEGIN")?;
        match conn.execute_batch(sql) {
            Ok(()) => {
                conn.execute(
                    "INSERT INTO _migrations(name, applied_at) VALUES (?1, ?2)",
                    rusqlite::params![name, current_timestamp()],
                )?;
                conn.execute_batch("COMMIT")?;
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e.into());
            }
        }
    }
    Ok(())
}

fn current_timestamp() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
