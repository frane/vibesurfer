//! `annotations` table CRUD.

use rusqlite::params;

use super::{epoch_secs, Store};
use crate::error::Result;
use crate::types::{Annotation, AnnotationTarget};

impl Store {
    pub fn add_annotation(
        &mut self,
        id: &str,
        target: &AnnotationTarget,
        key: &str,
        value: Option<&str>,
    ) -> Result<Annotation> {
        let now = epoch_secs();
        let kind = target.kind();
        let target_id = target.id();
        self.conn().execute(
            "INSERT INTO annotations(id, target_kind, target_id, key, value, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, kind, target_id, key, value, now],
        )?;
        Ok(Annotation {
            id: id.to_string(),
            target: target.clone(),
            key: key.to_string(),
            value: value.map(str::to_string),
            created_at: now,
        })
    }

    pub fn list_annotations(&self, target: &AnnotationTarget) -> Result<Vec<Annotation>> {
        let kind = target.kind();
        let target_id = target.id();
        let mut stmt = self.conn().prepare(
            "SELECT * FROM annotations
              WHERE target_kind=?1 AND target_id=?2
              ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([kind, target_id.as_str()], Annotation::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
