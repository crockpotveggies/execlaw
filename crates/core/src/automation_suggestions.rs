//! Suggestions storage for the `/automations` (Flows) landing page.
//!
//! Originally a bus-event sweeper: scan `state_bus_events`, group by
//! `(kind, source)`, surface high-volume patterns that no enabled
//! automation consumes. The bus + sweeper were torn out in the M6
//! rip-out (2026-05-22) — this module survives as the CRUD surface
//! the SPA's Suggestions section reads, and as the persisted store
//! that a future "detect untriaged chat-prompt patterns" middleware
//! sweeper can write into.
//!
//! Today's API:
//!   * `list_pending` / `get` — read paths the SPA hits.
//!   * `dismiss` / `mark_actioned` — operator actions from the list.
//!   * `list_muted` — read the muted-patterns table.
//!   * `set_draft_definition` — agent-drafted seed for the editor
//!     handoff. Future sweeper invokes this after a draft is ready.
//!
//! No write path exists from production code today; the table fills
//! from operator-driven dev/test imports, or from the future
//! middleware sweeper.

use crate::automations::AutomationDef;
use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuggestionStatus {
    /// Active suggestion — surfaces on the landing page.
    Pending,
    /// Operator dismissed it. The `(kind, source)` is also written
    /// into `state_automation_muted_patterns` so future sweeps skip.
    Dismissed,
    /// Operator clicked through to the editor and created an
    /// automation. We retain the historical row for telemetry but
    /// hide it from the suggestions list.
    Actioned,
}

impl SuggestionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Dismissed => "dismissed",
            Self::Actioned => "actioned",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "dismissed" => Some(Self::Dismissed),
            "actioned" => Some(Self::Actioned),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SuggestionRow {
    pub id: String,
    pub kind: String,
    pub source: String,
    pub event_count: i64,
    pub sample_event_ids: Vec<String>,
    pub suggested_name: String,
    pub status: SuggestionStatus,
    pub created_at: i64,
    pub updated_at: i64,
    /// Agent-drafted seed `AutomationDef` for the "Review and
    /// create" handoff. `None` for plain pattern-detected
    /// suggestions; `Some(_)` once an agent-drafting path populates
    /// the column. The editor pre-fills the JSON definition from
    /// this when present.
    #[serde(default)]
    pub draft_definition: Option<AutomationDef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutedPatternRow {
    pub kind: String,
    pub source: String,
    pub muted_at: i64,
}

#[derive(Debug, Error)]
pub enum SuggestionError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("encode: {0}")]
    Encode(#[from] serde_json::Error),
}

pub struct SuggestionStore<'a> {
    db: &'a Database,
}

impl<'a> SuggestionStore<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn list_pending(&self) -> Result<Vec<SuggestionRow>, SuggestionError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, kind, source, event_count, sample_event_ids, suggested_name, \
                        status, created_at, updated_at, draft_definition \
                 FROM state_automation_suggestions \
                 WHERE status = 'pending' \
                 ORDER BY event_count DESC, updated_at DESC",
            )?;
            let rows = stmt.query_map([], row_to_suggestion)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })?;
        Ok(rows)
    }

    pub fn get(&self, id: &str) -> Result<Option<SuggestionRow>, SuggestionError> {
        let row = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, kind, source, event_count, sample_event_ids, suggested_name, \
                        status, created_at, updated_at, draft_definition \
                 FROM state_automation_suggestions WHERE id = ?1",
            )?;
            let r = stmt.query_row([id], row_to_suggestion).ok();
            Ok(r)
        })?;
        Ok(row)
    }

    /// Dismiss a pending suggestion. Flips status to `dismissed` AND
    /// inserts the `(kind, source)` pair into the muted-patterns
    /// table so future sweepers (when the middleware redesign lands
    /// one) skip it.
    pub fn dismiss(&self, id: &str, now: i64) -> Result<bool, SuggestionError> {
        let row = match self.get(id)? {
            Some(r) => r,
            None => return Ok(false),
        };
        if !matches!(row.status, SuggestionStatus::Pending) {
            return Ok(false);
        }
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE state_automation_suggestions \
                 SET status = 'dismissed', updated_at = ?2 \
                 WHERE id = ?1",
                params![id, now],
            )?;
            c.execute(
                "INSERT INTO state_automation_muted_patterns (kind, source, muted_at) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT(kind, source) DO UPDATE SET muted_at = excluded.muted_at",
                params![&row.kind, &row.source, now],
            )?;
            Ok(())
        })?;
        Ok(true)
    }

    /// Mark a suggestion as actioned. Called by the API when the
    /// operator creates an automation from the suggestion's template.
    pub fn mark_actioned(&self, id: &str, now: i64) -> Result<bool, SuggestionError> {
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE state_automation_suggestions \
                 SET status = 'actioned', updated_at = ?2 \
                 WHERE id = ?1 AND status = 'pending'",
                params![id, now],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }

    pub fn list_muted(&self) -> Result<Vec<MutedPatternRow>, SuggestionError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT kind, source, muted_at FROM state_automation_muted_patterns \
                 ORDER BY muted_at DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(MutedPatternRow {
                    kind: r.get::<_, String>(0)?,
                    source: r.get(1)?,
                    muted_at: r.get(2)?,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })?;
        Ok(rows)
    }

    /// Persist an agent-drafted `AutomationDef` for a pending
    /// suggestion. The SPA's "Review and create" handoff reads the
    /// column and seeds the editor with the draft instead of an empty
    /// graph. Idempotent — passing the same id+def twice produces no
    /// further row changes. Returns `Ok(false)` when no pending
    /// suggestion exists for the id.
    pub fn set_draft_definition(
        &self,
        id: &str,
        def: &AutomationDef,
        now: i64,
    ) -> Result<bool, SuggestionError> {
        let payload = serde_json::to_string(def)?;
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE state_automation_suggestions \
                 SET draft_definition = ?2, updated_at = ?3 \
                 WHERE id = ?1 AND status = 'pending'",
                params![id, &payload, now],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }
}

fn row_to_suggestion(r: &rusqlite::Row) -> rusqlite::Result<SuggestionRow> {
    let sample_str: String = r.get(4)?;
    let sample_event_ids: Vec<String> = serde_json::from_str(&sample_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let status_str: String = r.get(6)?;
    let status = SuggestionStatus::parse(&status_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Text,
            format!("unknown suggestion status: {status_str}").into(),
        )
    })?;
    let draft_str: Option<String> = r.get(9)?;
    let draft_definition: Option<AutomationDef> = match draft_str {
        None => None,
        Some(s) if s.trim().is_empty() => None,
        Some(s) => Some(serde_json::from_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(e))
        })?),
    };
    Ok(SuggestionRow {
        id: r.get(0)?,
        kind: r.get::<_, String>(1)?,
        source: r.get(2)?,
        event_count: r.get(3)?,
        sample_event_ids,
        suggested_name: r.get(5)?,
        status,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
        draft_definition,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    /// Insert a row directly via SQL — sweepers don't exist after
    /// the M6 rip-out, so tests seed the store with the same shape a
    /// future middleware sweeper would write.
    fn insert_pending(db: &Database, id: &str, kind: &str, source: &str) {
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_automation_suggestions \
                 (id, kind, source, event_count, sample_event_ids, suggested_name, \
                  status, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, 10, '[]', 'Automate test', 'pending', 1000, 1000)",
                params![id, kind, source],
            )?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn list_pending_returns_inserted_rows() {
        let db = fresh_db();
        insert_pending(&db, "s-1", "webhook.received", "ring");
        insert_pending(&db, "s-2", "routine.fired", "morning-digest");
        let rows = SuggestionStore::new(&db).list_pending().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.id == "s-1" && r.kind == "webhook.received"));
        assert!(rows.iter().any(|r| r.id == "s-2" && r.kind == "routine.fired"));
    }

    #[test]
    fn dismiss_flips_status_and_records_muted_pattern() {
        let db = fresh_db();
        insert_pending(&db, "s-1", "webhook.received", "ring");
        let store = SuggestionStore::new(&db);
        assert!(store.dismiss("s-1", 9_999).unwrap());
        // Row no longer surfaces in pending.
        assert!(store.list_pending().unwrap().is_empty());
        // Muted-patterns row landed.
        let muted = store.list_muted().unwrap();
        assert_eq!(muted.len(), 1);
        assert_eq!(muted[0].kind, "webhook.received");
        assert_eq!(muted[0].source, "ring");
        // Idempotent: second dismiss returns false (already non-pending).
        assert!(!store.dismiss("s-1", 10_000).unwrap());
    }

    #[test]
    fn mark_actioned_only_acts_on_pending() {
        let db = fresh_db();
        insert_pending(&db, "s-1", "webhook.received", "ring");
        let store = SuggestionStore::new(&db);
        assert!(store.mark_actioned("s-1", 9_999).unwrap());
        // No longer pending → can't be actioned again.
        assert!(!store.mark_actioned("s-1", 10_000).unwrap());
    }

    #[test]
    fn set_draft_definition_persists_for_pending_only() {
        let db = fresh_db();
        insert_pending(&db, "s-1", "webhook.received", "ring");
        let store = SuggestionStore::new(&db);
        let def: AutomationDef = serde_json::from_value(serde_json::json!({
            "trigger": {"kind": "webhook.received", "when": null},
            "nodes": [],
            "edges": []
        }))
        .unwrap();
        assert!(store.set_draft_definition("s-1", &def, 9_999).unwrap());
        let row = store.get("s-1").unwrap().unwrap();
        assert!(row.draft_definition.is_some());
        // Flip to actioned: subsequent draft writes are refused.
        store.mark_actioned("s-1", 10_000).unwrap();
        assert!(!store.set_draft_definition("s-1", &def, 11_000).unwrap());
    }
}
