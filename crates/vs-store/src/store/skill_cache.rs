//! `skill_cache` table CRUD.

use rusqlite::params;

use super::Store;
use crate::error::Result;
use crate::types::SkillEntry;

impl Store {
    pub fn upsert_skill(&mut self, entry: &SkillEntry) -> Result<()> {
        self.conn().execute(
            "INSERT INTO skill_cache(name, version, sha, manifest, last_used_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(name) DO UPDATE
                SET version=excluded.version,
                    sha=excluded.sha,
                    manifest=excluded.manifest,
                    last_used_at=excluded.last_used_at",
            params![
                entry.name,
                entry.version,
                entry.sha,
                entry.manifest,
                entry.last_used_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_skill(&self, name: &str) -> Result<Option<SkillEntry>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT * FROM skill_cache WHERE name=?1")?;
        let mut rows = stmt.query([name])?;
        if let Some(row) = rows.next()? {
            Ok(Some(SkillEntry::from_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_skills(&self) -> Result<Vec<SkillEntry>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT * FROM skill_cache ORDER BY name ASC")?;
        let rows = stmt.query_map([], SkillEntry::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
