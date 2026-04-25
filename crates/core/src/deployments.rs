//! Runner-deployment registry (`config_runner_deployments` from migration 0001).
//!
//! A *deployment* is the operator-side mapping of a logical model
//! purpose (Standard / Reasoning / Guardrail / VoiceSTT / VoiceTTS) to
//! a concrete inference backend + endpoint + GPU pinning. The
//! container manager reads this on startup to know which backends to
//! launch; the runner reads it per turn to pick the deployment for
//! the current modality / capability tier.
//!
//! Phase 7 ships the CRUD store + routes + SPA editor. Today's MVP
//! enforces:
//!   - exactly-one `is_default = 1` per `purpose`,
//!   - well-known purpose strings (validated on insert/update),
//!   - `model_spec_json` is parsable JSON (the inference plugin
//!     defines the schema; we only check it parses).

use crate::db::{Database, DbError};
use crate::ids::DeploymentId;
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// Logical purpose of a deployment. Mirrors the comment in
/// `migration 0001`: `Standard | Reasoning | Guardrail | VoiceSTT | VoiceTTS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeploymentPurpose {
    Standard,
    Reasoning,
    Guardrail,
    VoiceStt,
    VoiceTts,
}

impl DeploymentPurpose {
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
}

/// One row in `config_runner_deployments`. `model_spec_json` is the
/// raw JSON blob — its schema is plugin-defined.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentRow {
    pub id: DeploymentId,
    pub purpose: DeploymentPurpose,
    /// Plugin id of the inference backend (e.g. `service-vllm`).
    pub inference_backend: String,
    pub model_spec: serde_json::Value,
    pub gpu_id: Option<String>,
    pub endpoint: Option<String>,
    pub is_default: bool,
    pub active: bool,
    pub notes: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Fields a caller may patch via `update`. `None` means "leave alone".
#[derive(Debug, Clone, Default)]
pub struct DeploymentPatch {
    pub purpose: Option<DeploymentPurpose>,
    pub inference_backend: Option<String>,
    pub model_spec: Option<serde_json::Value>,
    /// Outer Some = patch the column; inner Option = the new value
    /// (None clears the GPU pin / endpoint / notes).
    pub gpu_id: Option<Option<String>>,
    pub endpoint: Option<Option<String>>,
    pub is_default: Option<bool>,
    pub active: Option<bool>,
    pub notes: Option<Option<String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum DeploymentError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("deployment not found: {0}")]
    NotFound(String),
    #[error("invalid model_spec: {0}")]
    InvalidSpec(String),
    #[error("invalid purpose: {0}")]
    InvalidPurpose(String),
    #[error("inference_backend must not be empty")]
    EmptyBackend,
}

pub struct DeploymentStore<'db> {
    db: &'db Database,
}

impl<'db> DeploymentStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Insert a new deployment. If `is_default` is true, demotes any
    /// other row with the same purpose so the "exactly one default
    /// per purpose" invariant holds.
    pub fn insert(&self, row: &DeploymentRow) -> Result<(), DeploymentError> {
        if row.inference_backend.trim().is_empty() {
            return Err(DeploymentError::EmptyBackend);
        }
        let spec_blob = serde_json::to_vec(&row.model_spec).map_err(|e| {
            DeploymentError::InvalidSpec(format!("encode: {e}"))
        })?;
        let purpose = row.purpose.as_str();
        self.db.transaction(|tx| {
            if row.is_default {
                tx.execute(
                    "UPDATE config_runner_deployments SET is_default = 0 \
                     WHERE purpose = ?1 AND id != ?2",
                    params![purpose, row.id.as_str()],
                )?;
            }
            tx.execute(
                "INSERT INTO config_runner_deployments \
                 (id, purpose, inference_backend, model_spec_json, gpu_id, \
                  endpoint, is_default, active, notes, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    row.id.as_str(),
                    purpose,
                    row.inference_backend,
                    spec_blob,
                    row.gpu_id,
                    row.endpoint,
                    row.is_default as i64,
                    row.active as i64,
                    row.notes,
                    row.created_at,
                    row.updated_at,
                ],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    pub fn get(
        &self,
        id: &DeploymentId,
    ) -> Result<Option<DeploymentRow>, DeploymentError> {
        let row = self.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT id, purpose, inference_backend, model_spec_json, \
                            gpu_id, endpoint, is_default, active, notes, \
                            created_at, updated_at \
                     FROM config_runner_deployments WHERE id = ?1",
                    params![id.as_str()],
                    row_to_deployment,
                )
                .ok();
            Ok::<_, DbError>(got.transpose()?)
        })?;
        Ok(row)
    }

    pub fn list(&self) -> Result<Vec<DeploymentRow>, DeploymentError> {
        let rows: Vec<DeploymentRow> = self.db.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT id, purpose, inference_backend, model_spec_json, \
                        gpu_id, endpoint, is_default, active, notes, \
                        created_at, updated_at \
                 FROM config_runner_deployments \
                 ORDER BY purpose ASC, is_default DESC, created_at ASC",
            )?;
            let raw = stmt
                .query_map([], row_to_deployment)?
                .collect::<Result<Vec<_>, _>>()?;
            let parsed: Result<Vec<_>, rusqlite::Error> =
                raw.into_iter().collect();
            Ok::<_, DbError>(parsed?)
        })?;
        Ok(rows)
    }

    pub fn update(
        &self,
        id: &DeploymentId,
        patch: &DeploymentPatch,
        now: i64,
    ) -> Result<DeploymentRow, DeploymentError> {
        // Pre-validate any new fields that have invariants.
        if let Some(b) = &patch.inference_backend {
            if b.trim().is_empty() {
                return Err(DeploymentError::EmptyBackend);
            }
        }
        let new_spec_blob = match &patch.model_spec {
            Some(v) => Some(serde_json::to_vec(v).map_err(|e| {
                DeploymentError::InvalidSpec(format!("encode: {e}"))
            })?),
            None => None,
        };
        let id_str = id.as_str().to_owned();
        // Run the full mutation in a transaction so the
        // "exactly-one default per purpose" invariant doesn't leave
        // a transient violation visible to readers.
        let updated = self.db.transaction(|tx| {
            let existing: Option<(String, i64)> = tx
                .query_row(
                    "SELECT purpose, is_default FROM config_runner_deployments \
                     WHERE id = ?1",
                    params![id_str],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                )
                .ok();
            let Some((existing_purpose, _)) = existing else {
                return Err(DbError::Sqlite(rusqlite::Error::QueryReturnedNoRows));
            };
            let next_purpose = patch
                .purpose
                .map(|p| p.as_str().to_owned())
                .unwrap_or_else(|| existing_purpose.clone());

            // If the patch sets is_default=true, demote everyone else
            // sharing the (post-patch) purpose first.
            if patch.is_default == Some(true) {
                tx.execute(
                    "UPDATE config_runner_deployments SET is_default = 0 \
                     WHERE purpose = ?1 AND id != ?2",
                    params![next_purpose, id_str],
                )?;
            }

            // Build the UPDATE dynamically — easier to keep auditable
            // than COALESCE-everything.
            let mut sets: Vec<&'static str> = Vec::new();
            let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if let Some(p) = patch.purpose {
                sets.push("purpose = ?");
                binds.push(Box::new(p.as_str().to_owned()));
            }
            if let Some(b) = &patch.inference_backend {
                sets.push("inference_backend = ?");
                binds.push(Box::new(b.clone()));
            }
            if let Some(spec) = &new_spec_blob {
                sets.push("model_spec_json = ?");
                binds.push(Box::new(spec.clone()));
            }
            if let Some(gpu) = &patch.gpu_id {
                sets.push("gpu_id = ?");
                binds.push(Box::new(gpu.clone()));
            }
            if let Some(ep) = &patch.endpoint {
                sets.push("endpoint = ?");
                binds.push(Box::new(ep.clone()));
            }
            if let Some(d) = patch.is_default {
                sets.push("is_default = ?");
                binds.push(Box::new(d as i64));
            }
            if let Some(a) = patch.active {
                sets.push("active = ?");
                binds.push(Box::new(a as i64));
            }
            if let Some(notes) = &patch.notes {
                sets.push("notes = ?");
                binds.push(Box::new(notes.clone()));
            }
            sets.push("updated_at = ?");
            binds.push(Box::new(now));

            let mut sql = String::from("UPDATE config_runner_deployments SET ");
            sql.push_str(&sets.join(", "));
            sql.push_str(" WHERE id = ?");
            binds.push(Box::new(id_str.clone()));

            let params_vec: Vec<&dyn rusqlite::ToSql> =
                binds.iter().map(|b| b.as_ref()).collect();
            tx.execute(&sql, rusqlite::params_from_iter(params_vec.iter()))?;
            Ok(())
        });

        match updated {
            Ok(()) => self
                .get(id)?
                .ok_or_else(|| DeploymentError::NotFound(id.as_str().to_owned())),
            Err(DbError::Sqlite(rusqlite::Error::QueryReturnedNoRows)) => {
                Err(DeploymentError::NotFound(id.as_str().to_owned()))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn delete(&self, id: &DeploymentId) -> Result<(), DeploymentError> {
        let n = self.db.with_conn(|c| {
            Ok::<_, DbError>(c.execute(
                "DELETE FROM config_runner_deployments WHERE id = ?1",
                params![id.as_str()],
            )?)
        })?;
        if n == 0 {
            Err(DeploymentError::NotFound(id.as_str().to_owned()))
        } else {
            Ok(())
        }
    }
}

fn row_to_deployment(
    r: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<DeploymentRow, rusqlite::Error>> {
    let id: String = r.get(0)?;
    let purpose_str: String = r.get(1)?;
    let purpose = match DeploymentPurpose::parse(&purpose_str) {
        Some(p) => p,
        None => {
            return Ok(Err(rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown deployment purpose: {purpose_str}"),
                )),
            )));
        }
    };
    let inference_backend: String = r.get(2)?;
    let model_spec_blob: Vec<u8> = r.get(3)?;
    let model_spec: serde_json::Value =
        match serde_json::from_slice(&model_spec_blob) {
            Ok(v) => v,
            Err(_) => serde_json::Value::Null,
        };
    let gpu_id: Option<String> = r.get(4)?;
    let endpoint: Option<String> = r.get(5)?;
    let is_default: i64 = r.get(6)?;
    let active: i64 = r.get(7)?;
    let notes: Option<String> = r.get(8)?;
    let created_at: i64 = r.get(9)?;
    let updated_at: i64 = r.get(10)?;
    Ok(Ok(DeploymentRow {
        id: DeploymentId::from(id),
        purpose,
        inference_backend,
        model_spec,
        gpu_id,
        endpoint,
        is_default: is_default != 0,
        active: active != 0,
        notes,
        created_at,
        updated_at,
    }))
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

    fn mk_row(id: &str, purpose: DeploymentPurpose) -> DeploymentRow {
        DeploymentRow {
            id: DeploymentId::from(id),
            purpose,
            inference_backend: "service-vllm".into(),
            model_spec: serde_json::json!({"model": "Qwen3.5-27B-AWQ"}),
            gpu_id: None,
            endpoint: Some("http://127.0.0.1:8000/v1".into()),
            is_default: false,
            active: true,
            notes: None,
            created_at: 100,
            updated_at: 100,
        }
    }

    #[test]
    fn insert_and_get_round_trip() {
        let db = fresh_db();
        let store = DeploymentStore::new(&db);
        let row = mk_row("dep-1", DeploymentPurpose::Standard);
        store.insert(&row).unwrap();
        let got = store.get(&DeploymentId::from("dep-1")).unwrap().unwrap();
        assert_eq!(got.id.as_str(), "dep-1");
        assert_eq!(got.purpose, DeploymentPurpose::Standard);
        assert_eq!(got.model_spec["model"], "Qwen3.5-27B-AWQ");
    }

    #[test]
    fn list_returns_empty_then_ordered_by_purpose() {
        let db = fresh_db();
        let store = DeploymentStore::new(&db);
        assert!(store.list().unwrap().is_empty());
        store
            .insert(&mk_row("voice", DeploymentPurpose::VoiceStt))
            .unwrap();
        store
            .insert(&mk_row("std", DeploymentPurpose::Standard))
            .unwrap();
        let all = store.list().unwrap();
        assert_eq!(all.len(), 2);
        // Standard sorts before VoiceSTT alphabetically.
        assert_eq!(all[0].id.as_str(), "std");
        assert_eq!(all[1].id.as_str(), "voice");
    }

    #[test]
    fn empty_backend_is_rejected_on_insert_and_update() {
        let db = fresh_db();
        let store = DeploymentStore::new(&db);
        let mut bad = mk_row("dep-x", DeploymentPurpose::Standard);
        bad.inference_backend = "  ".into();
        assert!(matches!(
            store.insert(&bad),
            Err(DeploymentError::EmptyBackend)
        ));

        // Insert a valid one then try to patch it to empty.
        store.insert(&mk_row("dep-y", DeploymentPurpose::Standard)).unwrap();
        let err = store
            .update(
                &DeploymentId::from("dep-y"),
                &DeploymentPatch {
                    inference_backend: Some("".into()),
                    ..DeploymentPatch::default()
                },
                999,
            )
            .unwrap_err();
        assert!(matches!(err, DeploymentError::EmptyBackend));
    }

    #[test]
    fn setting_is_default_demotes_other_rows_with_same_purpose() {
        let db = fresh_db();
        let store = DeploymentStore::new(&db);
        let mut a = mk_row("a", DeploymentPurpose::Standard);
        a.is_default = true;
        store.insert(&a).unwrap();
        let mut b = mk_row("b", DeploymentPurpose::Standard);
        b.is_default = true; // demotes a on insert
        store.insert(&b).unwrap();
        let all = store.list().unwrap();
        let a_after = all.iter().find(|r| r.id.as_str() == "a").unwrap();
        let b_after = all.iter().find(|r| r.id.as_str() == "b").unwrap();
        assert!(!a_after.is_default);
        assert!(b_after.is_default);
        // Promote `a` via patch — `b` flips back.
        store
            .update(
                &DeploymentId::from("a"),
                &DeploymentPatch {
                    is_default: Some(true),
                    ..DeploymentPatch::default()
                },
                200,
            )
            .unwrap();
        let all = store.list().unwrap();
        let a_after = all.iter().find(|r| r.id.as_str() == "a").unwrap();
        let b_after = all.iter().find(|r| r.id.as_str() == "b").unwrap();
        assert!(a_after.is_default);
        assert!(!b_after.is_default);
    }

    #[test]
    fn defaults_are_per_purpose_not_global() {
        let db = fresh_db();
        let store = DeploymentStore::new(&db);
        let mut std_default = mk_row("std", DeploymentPurpose::Standard);
        std_default.is_default = true;
        let mut voice_default = mk_row("voice", DeploymentPurpose::VoiceStt);
        voice_default.is_default = true;
        store.insert(&std_default).unwrap();
        store.insert(&voice_default).unwrap();
        // Both rows keep is_default=true since they're distinct purposes.
        let all = store.list().unwrap();
        let std = all.iter().find(|r| r.id.as_str() == "std").unwrap();
        let voice = all.iter().find(|r| r.id.as_str() == "voice").unwrap();
        assert!(std.is_default);
        assert!(voice.is_default);
    }

    #[test]
    fn update_patches_only_supplied_fields() {
        let db = fresh_db();
        let store = DeploymentStore::new(&db);
        store.insert(&mk_row("dep-u", DeploymentPurpose::Standard)).unwrap();
        let updated = store
            .update(
                &DeploymentId::from("dep-u"),
                &DeploymentPatch {
                    notes: Some(Some("a note".into())),
                    active: Some(false),
                    ..DeploymentPatch::default()
                },
                500,
            )
            .unwrap();
        assert_eq!(updated.notes.as_deref(), Some("a note"));
        assert!(!updated.active);
        assert_eq!(updated.updated_at, 500);
        // Untouched fields preserved.
        assert_eq!(updated.inference_backend, "service-vllm");
    }

    #[test]
    fn update_unknown_id_returns_not_found() {
        let db = fresh_db();
        let store = DeploymentStore::new(&db);
        let err = store
            .update(
                &DeploymentId::from("missing"),
                &DeploymentPatch {
                    notes: Some(Some("x".into())),
                    ..DeploymentPatch::default()
                },
                1,
            )
            .unwrap_err();
        assert!(matches!(err, DeploymentError::NotFound(_)));
    }

    #[test]
    fn delete_removes_row_and_returns_not_found_on_repeat() {
        let db = fresh_db();
        let store = DeploymentStore::new(&db);
        store
            .insert(&mk_row("dep-del", DeploymentPurpose::Reasoning))
            .unwrap();
        store.delete(&DeploymentId::from("dep-del")).unwrap();
        assert!(store.get(&DeploymentId::from("dep-del")).unwrap().is_none());
        let err = store.delete(&DeploymentId::from("dep-del")).unwrap_err();
        assert!(matches!(err, DeploymentError::NotFound(_)));
    }

    #[test]
    fn purpose_round_trips_via_str() {
        for p in [
            DeploymentPurpose::Standard,
            DeploymentPurpose::Reasoning,
            DeploymentPurpose::Guardrail,
            DeploymentPurpose::VoiceStt,
            DeploymentPurpose::VoiceTts,
        ] {
            assert_eq!(DeploymentPurpose::parse(p.as_str()), Some(p));
        }
        assert_eq!(DeploymentPurpose::parse("Mystery"), None);
    }
}
