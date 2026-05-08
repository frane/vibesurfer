//! `actions` table: audit log + idempotency cache lookup.

use rusqlite::params;

use super::Store;
use crate::error::Result;
use crate::types::{Action, ActionFilter, ActionInsert};

impl Store {
    /// Insert one row into `actions`. Returns the autoincrement id.
    pub fn record_action(&mut self, ins: &ActionInsert) -> Result<i64> {
        self.conn().execute(
            "INSERT INTO actions(
                session_id, page_id, primitive, args_redacted, args_hash,
                before_token, after_token, idempotency_hit, result_summary,
                latency_ms, group_label, started_at, finished_at, error_code
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                ins.session_id,
                ins.page_id,
                ins.primitive,
                ins.args_redacted,
                ins.args_hash,
                ins.before_token,
                ins.after_token,
                i64::from(ins.idempotency_hit),
                ins.result_summary,
                ins.latency_ms,
                ins.group_label,
                ins.started_at,
                ins.finished_at,
                ins.error_code,
            ],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    /// Look up the most recent successful action whose
    /// `(page_id, before_token, args_hash)` triple matches and whose
    /// `started_at` is within `ttl_secs` of `now_secs`. The caller
    /// supplies `now_secs` for testability; production calls
    /// [`super::epoch_secs`].
    pub fn lookup_idempotent(
        &self,
        page_id: &str,
        before_token: &str,
        args_hash: &str,
        now_secs: i64,
        ttl_secs: i64,
    ) -> Result<Option<Action>> {
        let mut stmt = self.conn().prepare(
            "SELECT * FROM actions
              WHERE page_id=?1 AND before_token=?2 AND args_hash=?3
                AND error_code IS NULL
                AND idempotency_hit=0
                AND started_at >= ?4
              ORDER BY started_at DESC
              LIMIT 1",
        )?;
        let cutoff = now_secs - ttl_secs;
        let mut rows = stmt.query(params![page_id, before_token, args_hash, cutoff])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Action::from_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_actions(&self, filter: &ActionFilter) -> Result<Vec<Action>> {
        use std::fmt::Write as _;
        let mut sql = String::from("SELECT * FROM actions WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        let mut idx = 1usize;
        if let Some(s) = &filter.session_id {
            write!(sql, " AND session_id=?{idx}").expect("write to String");
            args.push(Box::new(s.clone()));
            idx += 1;
        }
        if let Some(p) = &filter.page_id {
            write!(sql, " AND page_id=?{idx}").expect("write to String");
            args.push(Box::new(p.clone()));
            idx += 1;
        }
        if let Some(g) = &filter.group_label {
            write!(sql, " AND group_label=?{idx}").expect("write to String");
            args.push(Box::new(g.clone()));
            idx += 1;
        }
        if let Some(t) = filter.since_started_at {
            write!(sql, " AND started_at >= ?{idx}").expect("write to String");
            args.push(Box::new(t));
            idx += 1;
        }
        sql.push_str(" ORDER BY started_at ASC");
        if let Some(limit) = filter.limit {
            write!(sql, " LIMIT ?{idx}").expect("write to String");
            args.push(Box::new(limit));
        }
        let mut stmt = self.conn().prepare(&sql)?;
        let arg_refs: Vec<&dyn rusqlite::ToSql> =
            args.iter().map(std::convert::AsRef::as_ref).collect();
        let rows = stmt.query_map(arg_refs.as_slice(), Action::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
