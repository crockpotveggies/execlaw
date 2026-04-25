//! Config-audit log accessor (`config_audit` table from migration 0001).
//!
//! Every mutation to a `config_*` table is recorded here with the
//! actor that performed it. The Rust repository layer is responsible
//! for writing entries — triggers can't see the user-supplied actor.
//!
//! Phase 6b only needs the read path: the SPA's Settings → Audit page
//! reads from this. Writers land alongside each `config_*` mutation
//! route in Phase 7 (deployment editor) and beyond.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// One row out of `config_audit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    pub ts: i64,
    pub actor: String,
    pub table_name: String,
    pub row_id: String,
    /// Old row encoded as JSON, if any. None for INSERT operations.
    pub old_json: Option<serde_json::Value>,
    /// New row encoded as JSON, if any. None for DELETE operations.
    pub new_json: Option<serde_json::Value>,
}

/// Repository for `config_audit`.
pub struct AuditStore<'db> {
    db: &'db Database,
}

impl<'db> AuditStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Insert one audit entry. `old_json` / `new_json` are serialized
    /// here so callers can pass typed payloads.
    pub fn insert(
        &self,
        actor: &str,
        table_name: &str,
        row_id: &str,
        old: Option<&serde_json::Value>,
        new: Option<&serde_json::Value>,
    ) -> Result<i64, DbError> {
        let now = chrono::Utc::now().timestamp();
        let old_blob = match old {
            Some(v) => Some(serde_json::to_vec(v).map_err(|e| {
                DbError::Serde(format!("encoding old_json: {e}"))
            })?),
            None => None,
        };
        let new_blob = match new {
            Some(v) => Some(serde_json::to_vec(v).map_err(|e| {
                DbError::Serde(format!("encoding new_json: {e}"))
            })?),
            None => None,
        };
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO config_audit (ts, actor, table_name, row_id, old_json, new_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![now, actor, table_name, row_id, old_blob, new_blob],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    /// Recent entries, newest-first. Optional `since_ts` lower bound
    /// (inclusive) and a hard cap; the SPA paginates by ts.
    pub fn list(
        &self,
        since_ts: Option<i64>,
        limit: i64,
    ) -> Result<Vec<AuditEntry>, DbError> {
        let limit = limit.clamp(1, 1000);
        self.db.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT id, ts, actor, table_name, row_id, old_json, new_json \
                 FROM config_audit \
                 WHERE (?1 IS NULL OR ts >= ?1) \
                 ORDER BY ts DESC, id DESC \
                 LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![since_ts, limit], |r| {
                    let old_blob: Option<Vec<u8>> = r.get(5)?;
                    let new_blob: Option<Vec<u8>> = r.get(6)?;
                    Ok(AuditEntry {
                        id: r.get(0)?,
                        ts: r.get(1)?,
                        actor: r.get(2)?,
                        table_name: r.get(3)?,
                        row_id: r.get(4)?,
                        old_json: old_blob
                            .as_deref()
                            .and_then(|b| serde_json::from_slice(b).ok()),
                        new_json: new_blob
                            .as_deref()
                            .and_then(|b| serde_json::from_slice(b).ok()),
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }
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

    #[test]
    fn list_returns_empty_on_fresh_db() {
        let db = fresh_db();
        let store = AuditStore::new(&db);
        assert!(store.list(None, 100).unwrap().is_empty());
    }

    #[test]
    fn insert_then_list_round_trips_payloads() {
        let db = fresh_db();
        let store = AuditStore::new(&db);
        let id = store
            .insert(
                "controller-1",
                "config_runner_deployments",
                "row-x",
                None,
                Some(&serde_json::json!({"k": "v"})),
            )
            .unwrap();
        assert!(id > 0);
        let rows = store.list(None, 10).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.actor, "controller-1");
        assert_eq!(r.table_name, "config_runner_deployments");
        assert!(r.old_json.is_none());
        assert_eq!(r.new_json.as_ref().unwrap()["k"], "v");
    }

    #[test]
    fn list_orders_newest_first_and_respects_limit() {
        let db = fresh_db();
        let store = AuditStore::new(&db);
        for i in 0..5 {
            store
                .insert(
                    "ctrl",
                    "config_alert_routing",
                    &format!("k{i}"),
                    None,
                    Some(&serde_json::json!({"i": i})),
                )
                .unwrap();
        }
        let rows = store.list(None, 3).unwrap();
        assert_eq!(rows.len(), 3);
        // Newest first: id descending, ts descending.
        for w in rows.windows(2) {
            assert!(w[0].id >= w[1].id);
        }
    }

    #[test]
    fn since_ts_filters_older_rows() {
        let db = fresh_db();
        let store = AuditStore::new(&db);
        store
            .insert("a", "t", "old", None, None)
            .unwrap();
        // Far-future since_ts should exclude every row.
        let rows = store.list(Some(9_999_999_999), 100).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn list_clamps_limit_into_valid_range() {
        let db = fresh_db();
        let store = AuditStore::new(&db);
        // 0 / negative limits clamp to 1.
        let _ = store.list(None, 0).unwrap();
        let _ = store.list(None, -5).unwrap();
        // Excessive limit clamps to 1000.
        let _ = store.list(None, 100_000).unwrap();
    }
}
