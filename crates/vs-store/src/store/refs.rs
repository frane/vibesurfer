//! `refs` table CRUD.

use rusqlite::params;

use super::{epoch_secs, Store};
use crate::error::{Result, StoreError};
use crate::types::StoredRef;

impl Store {
    pub fn record_ref(&mut self, sr: &StoredRef) -> Result<()> {
        self.conn().execute(
            "INSERT INTO refs(session_id, page_id, ref, dom_path, role,
                              content_hash, created_at, retired_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                sr.session_id,
                sr.page_id,
                i64::from(sr.r),
                sr.dom_path,
                sr.role,
                sr.content_hash,
                sr.created_at,
                sr.retired_at,
            ],
        )?;
        Ok(())
    }

    pub fn retire_ref(&mut self, session_id: &str, page_id: &str, r: u32) -> Result<()> {
        let now = epoch_secs();
        let n = self.conn().execute(
            "UPDATE refs
                SET retired_at=?4
              WHERE session_id=?1 AND page_id=?2 AND ref=?3 AND retired_at IS NULL",
            params![session_id, page_id, i64::from(r), now],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound {
                kind: "ref",
                id: format!("{session_id}/{page_id}/{r}"),
            });
        }
        Ok(())
    }

    pub fn list_refs(&self, session_id: &str, page_id: &str) -> Result<Vec<StoredRef>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT * FROM refs WHERE session_id=?1 AND page_id=?2 ORDER BY ref ASC")?;
        let rows = stmt.query_map([session_id, page_id], StoredRef::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
