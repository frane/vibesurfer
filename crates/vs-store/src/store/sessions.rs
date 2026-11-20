//! `sessions` table CRUD.

use rusqlite::params;

use super::{epoch_secs, Store};
use crate::error::{Result, StoreError};
use crate::types::{Session, SessionStatus};

impl Store {
    /// Insert a new session row and return it.
    pub fn create_session(&mut self, id: &str, policy_id: Option<&str>) -> Result<Session> {
        self.create_session_with(id, policy_id, false, false)
    }

    /// Insert a new session row, optionally flagged `isolated` (its
    /// pages get a private, non-persistent browser data store) and
    /// `intercept` (its pages go through the daemon's HTTP proxy).
    pub fn create_session_with(
        &mut self,
        id: &str,
        policy_id: Option<&str>,
        isolated: bool,
        intercept: bool,
    ) -> Result<Session> {
        let now = epoch_secs();
        self.conn().execute(
            "INSERT INTO sessions(id, created_at, policy_id, status, closed_at, isolated, intercept)
             VALUES (?1, ?2, ?3, 'open', NULL, ?4, ?5)",
            params![id, now, policy_id, i64::from(isolated), i64::from(intercept)],
        )?;
        Ok(Session {
            id: id.to_string(),
            created_at: now,
            policy_id: policy_id.map(str::to_string),
            status: SessionStatus::Open,
            closed_at: None,
            isolated,
            intercept,
        })
    }

    /// Mark a session closed (and stamp `closed_at`). Returns the row.
    pub fn close_session(&mut self, id: &str) -> Result<Session> {
        let now = epoch_secs();
        let n = self.conn().execute(
            "UPDATE sessions SET status='closed', closed_at=?2 WHERE id=?1 AND status='open'",
            params![id, now],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound {
                kind: "session",
                id: id.to_string(),
            });
        }
        self.get_session(id)?.ok_or(StoreError::NotFound {
            kind: "session",
            id: id.to_string(),
        })
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let mut stmt = self.conn().prepare("SELECT * FROM sessions WHERE id=?1")?;
        let mut rows = stmt.query([id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Session::from_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT * FROM sessions ORDER BY created_at DESC")?;
        let rows = stmt.query_map([], Session::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
