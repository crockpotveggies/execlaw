//! Inference-backend-per-purpose registry (`config_backends`).
//!
//! A *backend* is the operator-side mapping of a logical model
//! purpose (Standard / Reasoning / Guardrail / VoiceSTT / VoiceTTS)
//! to a concrete inference backend + endpoint + GPU pinning. The
//! container manager reads this on startup to know which backends
//! to launch; the runner reads it per turn to pick the backend for
//! the current modality / capability tier.
//!
//! There is **exactly one backend per purpose**, period. The set of
//! purposes is a fixed enum, so the table's PK is `purpose` itself
//! and there is no add/delete affordance — only edit. See
//! `docs/runner-design.md` for why the previous "deployment" CRUD
//! shape was the wrong abstraction.
//!
//! Note: this module replaces the legacy `deployments.rs` after the
//! Phase 8.5 rename. The old `DeploymentRow` / `DeploymentStore` /
//! `DeploymentPurpose` types are gone.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Logical purpose served by a backend. Mirrors the comment in
/// migration 0001 and the locked decisions in §2.13.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum BackendPurpose {
    Standard,
    Reasoning,
    Guardrail,
    VoiceStt,
    VoiceTts,
}

impl BackendPurpose {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Standard => "Standard",
            Self::Reasoning => "Reasoning",
            Self::Guardrail => "Guardrail",
            Self::VoiceStt => "VoiceSTT",
            Self::VoiceTts => "VoiceTTS",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Standard" => Some(Self::Standard),
            "Reasoning" => Some(Self::Reasoning),
            "Guardrail" => Some(Self::Guardrail),
            "VoiceSTT" => Some(Self::VoiceStt),
            "VoiceTTS" => Some(Self::VoiceTts),
            _ => None,
        }
    }

    /// Every purpose execlaw recognises. The Settings UI iterates
    /// this so a missing row renders as "not configured" instead of
    /// silently disappearing.
    pub fn all() -> &'static [BackendPurpose] {
        &[
            Self::Standard,
            Self::Reasoning,
            Self::Guardrail,
            Self::VoiceStt,
            Self::VoiceTts,
        ]
    }
}

/// One row in `config_backends`. `model_spec_json` is the raw JSON
/// blob — its schema is plugin-defined.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackendRow {
    pub purpose: BackendPurpose,
    pub inference_backend: String,
    pub model_spec_json: serde_json::Value,
    pub gpu_id: Option<String>,
    pub endpoint: Option<String>,
    pub notes: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Operator-supplied form payload. Same shape as `BackendRow`
/// minus the timestamps the store fills in.
#[derive(Debug, Clone)]
pub struct BackendUpsert {
    pub purpose: BackendPurpose,
    pub inference_backend: String,
    pub model_spec_json: serde_json::Value,
    pub gpu_id: Option<String>,
    pub endpoint: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Error)]
pub enum BackendError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("invalid backend payload: {0}")]
    Invalid(String),
    #[error("rusqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("no backend configured for purpose {0}")]
    NotFound(String),
}

pub struct BackendStore<'db> {
    db: &'db Database,
}

impl<'db> BackendStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Upsert by purpose. Insert on first sight, update on
    /// subsequent calls. The operator never adds or deletes a
    /// backend — purposes are a fixed enum.
    pub fn upsert(&self, payload: &BackendUpsert, now: i64) -> Result<BackendRow, BackendError> {
        let model_blob = serde_json::to_vec(&payload.model_spec_json)
            .map_err(|e| BackendError::Invalid(format!("model_spec_json: {e}")))?;
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO config_backends \
                   (purpose, inference_backend, model_spec_json, gpu_id, endpoint, notes, \
                    created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7) \
                 ON CONFLICT(purpose) DO UPDATE SET \
                    inference_backend = excluded.inference_backend, \
                    model_spec_json   = excluded.model_spec_json, \
                    gpu_id            = excluded.gpu_id, \
                    endpoint          = excluded.endpoint, \
                    notes             = excluded.notes, \
                    updated_at        = excluded.updated_at",
                params![
                    payload.purpose.as_str(),
                    payload.inference_backend,
                    model_blob,
                    payload.gpu_id,
                    payload.endpoint,
                    payload.notes,
                    now,
                ],
            )?;
            Ok(())
        })?;
        self.get(payload.purpose)?
            .ok_or(BackendError::NotFound(payload.purpose.as_str().to_owned()))
    }

    pub fn get(&self, purpose: BackendPurpose) -> Result<Option<BackendRow>, BackendError> {
        let row = self.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT purpose, inference_backend, model_spec_json, gpu_id, endpoint, \
                            notes, created_at, updated_at \
                     FROM config_backends WHERE purpose = ?1",
                    params![purpose.as_str()],
                    row_to_backend,
                )
                .ok();
            Ok(got)
        })?;
        Ok(row)
    }

    /// List every configured backend, ordered by purpose. Missing
    /// purposes are omitted; the UI fills in placeholders by
    /// iterating `BackendPurpose::all()`.
    pub fn list_all(&self) -> Result<Vec<BackendRow>, BackendError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT purpose, inference_backend, model_spec_json, gpu_id, endpoint, \
                        notes, created_at, updated_at \
                 FROM config_backends \
                 ORDER BY purpose ASC",
            )?;
            let rows = stmt
                .query_map([], row_to_backend)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows)
    }

    /// Operator can clear a configured backend without inserting a
    /// new one — useful when wiping a misconfigured Reasoning slot
    /// without forcing the operator to type a placeholder. Returns
    /// `true` when a row was actually removed.
    pub fn clear(&self, purpose: BackendPurpose) -> Result<bool, BackendError> {
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM config_backends WHERE purpose = ?1",
                params![purpose.as_str()],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }
}

fn row_to_backend(row: &rusqlite::Row<'_>) -> rusqlite::Result<BackendRow> {
    let purpose_str: String = row.get(0)?;
    let purpose = BackendPurpose::parse(&purpose_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown backend purpose: {purpose_str}"),
            )),
        )
    })?;
    let model_blob: Vec<u8> = row.get(2)?;
    let model_spec_json: serde_json::Value = serde_json::from_slice(&model_blob)
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
    Ok(BackendRow {
        purpose,
        inference_backend: row.get(1)?,
        model_spec_json,
        gpu_id: row.get(3)?,
        endpoint: row.get(4)?,
        notes: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn upsert_payload(purpose: BackendPurpose) -> BackendUpsert {
        BackendUpsert {
            purpose,
            inference_backend: "service-vllm".into(),
            model_spec_json: serde_json::json!({"model": "Qwen3.5-27B-AWQ"}),
            gpu_id: Some("0".into()),
            endpoint: Some("http://127.0.0.1:8000/v1".into()),
            notes: None,
        }
    }

    #[test]
    fn upsert_inserts_then_updates_in_place() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        let r1 = store.upsert(&upsert_payload(BackendPurpose::Standard), 100).unwrap();
        assert_eq!(r1.created_at, 100);
        assert_eq!(r1.updated_at, 100);
        let mut p2 = upsert_payload(BackendPurpose::Standard);
        p2.endpoint = Some("http://127.0.0.1:9000/v1".into());
        let r2 = store.upsert(&p2, 200).unwrap();
        assert_eq!(r2.endpoint.as_deref(), Some("http://127.0.0.1:9000/v1"));
        assert_eq!(r2.created_at, 100, "first-insert timestamp survives update");
        assert_eq!(r2.updated_at, 200);
    }

    #[test]
    fn list_all_orders_by_purpose() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        store
            .upsert(&upsert_payload(BackendPurpose::VoiceTts), 100)
            .unwrap();
        store
            .upsert(&upsert_payload(BackendPurpose::Standard), 100)
            .unwrap();
        store
            .upsert(&upsert_payload(BackendPurpose::Reasoning), 100)
            .unwrap();
        let purposes: Vec<_> = store
            .list_all()
            .unwrap()
            .into_iter()
            .map(|r| r.purpose.as_str())
            .collect();
        // Alphabetical by string, which is the SQLite ORDER BY.
        assert_eq!(purposes, vec!["Reasoning", "Standard", "VoiceTTS"]);
    }

    #[test]
    fn clear_removes_one_purpose_only() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        store.upsert(&upsert_payload(BackendPurpose::Standard), 0).unwrap();
        store.upsert(&upsert_payload(BackendPurpose::Reasoning), 0).unwrap();
        assert!(store.clear(BackendPurpose::Standard).unwrap());
        assert!(store.get(BackendPurpose::Standard).unwrap().is_none());
        assert!(store.get(BackendPurpose::Reasoning).unwrap().is_some());
        // Re-clearing returns false.
        assert!(!store.clear(BackendPurpose::Standard).unwrap());
    }

    #[test]
    fn purpose_all_lists_every_enum_value() {
        let names: Vec<_> = BackendPurpose::all().iter().map(|p| p.as_str()).collect();
        assert!(names.contains(&"Standard"));
        assert!(names.contains(&"Reasoning"));
        assert!(names.contains(&"Guardrail"));
        assert!(names.contains(&"VoiceSTT"));
        assert!(names.contains(&"VoiceTTS"));
        assert_eq!(names.len(), 5);
    }

    #[test]
    fn parse_round_trips() {
        for p in BackendPurpose::all() {
            assert_eq!(BackendPurpose::parse(p.as_str()), Some(*p));
        }
        assert_eq!(BackendPurpose::parse("nope"), None);
    }
}
