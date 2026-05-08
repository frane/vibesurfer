//! `marks` table CRUD.

use rusqlite::params;

use super::{epoch_secs, Store};
use crate::error::{Result, StoreError};
use crate::types::Mark;

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub fn create_mark(
        &mut self,
        id: &str,
        session_id: &str,
        page_id: &str,
        name: &str,
        dom_path: &str,
        role: Option<&str>,
        content_excerpt: Option<&str>,
    ) -> Result<Mark> {
        let now = epoch_secs();
        self.conn()
            .execute(
                "INSERT INTO marks(id, session_id, page_id, name, dom_path,
                                   role, content_excerpt, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id,
                    session_id,
                    page_id,
                    name,
                    dom_path,
                    role,
                    content_excerpt,
                    now
                ],
            )
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(err, _)
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    StoreError::Conflict("mark name already in session")
                }
                other => StoreError::Sqlite(other),
            })?;
        Ok(Mark {
            id: id.to_string(),
            session_id: session_id.to_string(),
            page_id: page_id.to_string(),
            name: name.to_string(),
            dom_path: dom_path.to_string(),
            role: role.map(str::to_string),
            content_excerpt: content_excerpt.map(str::to_string),
            created_at: now,
        })
    }

    pub fn get_mark(&self, session_id: &str, name: &str) -> Result<Option<Mark>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT * FROM marks WHERE session_id=?1 AND name=?2")?;
        let mut rows = stmt.query([session_id, name])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Mark::from_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_marks(&self, session_id: &str) -> Result<Vec<Mark>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT * FROM marks WHERE session_id=?1 ORDER BY created_at ASC")?;
        let rows = stmt.query_map([session_id], Mark::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delete_mark(&mut self, session_id: &str, name: &str) -> Result<()> {
        let n = self.conn().execute(
            "DELETE FROM marks WHERE session_id=?1 AND name=?2",
            params![session_id, name],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound {
                kind: "mark",
                id: format!("{session_id}/{name}"),
            });
        }
        Ok(())
    }
}
