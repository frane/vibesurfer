//! `pages` table CRUD.

use rusqlite::params;

use super::{epoch_secs, Store};
use crate::error::{Result, StoreError};
use crate::types::Page;

impl Store {
    pub fn create_page(&mut self, id: &str, session_id: &str, url: &str) -> Result<Page> {
        let now = epoch_secs();
        self.conn().execute(
            "INSERT INTO pages(id, session_id, url, title, last_token, last_dom_hash,
                               last_seen_at, closed_at)
             VALUES (?1, ?2, ?3, NULL, NULL, NULL, ?4, NULL)",
            params![id, session_id, url, now],
        )?;
        Ok(Page {
            id: id.to_string(),
            session_id: session_id.to_string(),
            url: url.to_string(),
            title: None,
            last_token: None,
            last_dom_hash: None,
            last_seen_at: now,
            closed_at: None,
        })
    }

    pub fn close_page(&mut self, id: &str) -> Result<()> {
        let now = epoch_secs();
        let n = self.conn().execute(
            "UPDATE pages SET closed_at=?2 WHERE id=?1 AND closed_at IS NULL",
            params![id, now],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound {
                kind: "page",
                id: id.to_string(),
            });
        }
        Ok(())
    }

    pub fn update_page_token(
        &mut self,
        id: &str,
        token: &str,
        dom_hash: &str,
        title: Option<&str>,
    ) -> Result<()> {
        let now = epoch_secs();
        let n = self.conn().execute(
            "UPDATE pages
                SET last_token=?2, last_dom_hash=?3, last_seen_at=?4,
                    title=COALESCE(?5, title)
              WHERE id=?1",
            params![id, token, dom_hash, now, title],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound {
                kind: "page",
                id: id.to_string(),
            });
        }
        Ok(())
    }

    /// Update a page's URL after an in-place navigation (`vs_goto`), so
    /// the persisted row and any later resurrection reflect the current
    /// document.
    pub fn update_page_url(&mut self, id: &str, url: &str) -> Result<()> {
        let now = epoch_secs();
        let n = self.conn().execute(
            "UPDATE pages SET url=?2, last_seen_at=?3 WHERE id=?1",
            params![id, url, now],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound {
                kind: "page",
                id: id.to_string(),
            });
        }
        Ok(())
    }

    pub fn get_page(&self, id: &str) -> Result<Option<Page>> {
        let mut stmt = self.conn().prepare("SELECT * FROM pages WHERE id=?1")?;
        let mut rows = stmt.query([id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Page::from_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_pages(&self, session_id: &str) -> Result<Vec<Page>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT * FROM pages WHERE session_id=?1 ORDER BY last_seen_at DESC")?;
        let rows = stmt.query_map([session_id], Page::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
