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

/// Logical purpose served by a backend. Post-Phase-8.8 reshape:
/// Guardrail (unused; one-model world) and Reasoning (capability,
/// not a deployment) are gone. Reasoning is now a flag on Standard.
/// Small was added for voice mode and any latency-sensitive route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum BackendPurpose {
    Standard,
    Small,
    VoiceStt,
    VoiceTts,
}

/// Lifecycle ownership for a backend (Phase 12 — §5.4).
///
///   * `External` — operator points us at a URL they manage. Control
///     plane never touches the process; we just call the endpoint.
///   * `Managed` — control plane spawns + supervises the inference
///     container. `endpoint` is computed at spawn time and written
///     back to the row; operator picks image tag + cmd args in
///     `model_spec_json`. The supervisor lives in the server crate
///     (BackendSupervisor); the storage layer is mode-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum BackendMode {
    External,
    Managed,
}

impl BackendMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::External => "external",
            Self::Managed => "managed",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "external" => Some(Self::External),
            "managed" => Some(Self::Managed),
            _ => None,
        }
    }
}

impl BackendPurpose {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Standard => "Standard",
            Self::Small => "Small",
            Self::VoiceStt => "VoiceSTT",
            Self::VoiceTts => "VoiceTTS",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Standard" => Some(Self::Standard),
            "Small" => Some(Self::Small),
            "VoiceSTT" => Some(Self::VoiceStt),
            "VoiceTTS" => Some(Self::VoiceTts),
            _ => None,
        }
    }

    /// Every purpose execlaw recognises. The Settings UI iterates
    /// this so a missing row renders as "not configured" instead of
    /// silently disappearing.
    pub fn all() -> &'static [BackendPurpose] {
        &[Self::Standard, Self::Small, Self::VoiceStt, Self::VoiceTts]
    }

    /// `reasoning_enabled` only applies to the Standard purpose —
    /// other purposes don't expose a reasoning toggle. Small is
    /// always non-reasoning by design (it's the fast path); voice
    /// purposes don't run an LLM in the reasoning sense.
    pub fn supports_reasoning_toggle(self) -> bool {
        matches!(self, Self::Standard)
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
    /// Phase-8.8: whether the runner should engage the model's
    /// native reasoning mode (Qwen3.5 `<think>` blocks etc.) on this
    /// backend. Only meaningful for Standard. Persisted so the
    /// runner doesn't have to look up a separate config.
    pub reasoning_enabled: bool,
    /// Phase 12 — lifecycle ownership. Defaults to `External` so
    /// every existing row keeps its current behaviour after the
    /// migration. The supervisor only acts on `Managed` rows.
    pub mode: BackendMode,
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
    pub reasoning_enabled: bool,
    pub mode: BackendMode,
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
    /// backend — purposes are a fixed enum. `reasoning_enabled`
    /// is silently zeroed for purposes that don't support the
    /// toggle (Small / VoiceSTT / VoiceTTS) so a stale boolean
    /// doesn't end up in the runner's config.
    pub fn upsert(&self, payload: &BackendUpsert, now: i64) -> Result<BackendRow, BackendError> {
        let model_blob = serde_json::to_vec(&payload.model_spec_json)
            .map_err(|e| BackendError::Invalid(format!("model_spec_json: {e}")))?;
        let reasoning = payload.reasoning_enabled
            && payload.purpose.supports_reasoning_toggle();
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO config_backends \
                   (purpose, inference_backend, model_spec_json, gpu_id, endpoint, notes, \
                    reasoning_enabled, mode, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9) \
                 ON CONFLICT(purpose) DO UPDATE SET \
                    inference_backend = excluded.inference_backend, \
                    model_spec_json   = excluded.model_spec_json, \
                    gpu_id            = excluded.gpu_id, \
                    endpoint          = excluded.endpoint, \
                    notes             = excluded.notes, \
                    reasoning_enabled = excluded.reasoning_enabled, \
                    mode              = excluded.mode, \
                    updated_at        = excluded.updated_at",
                params![
                    payload.purpose.as_str(),
                    payload.inference_backend,
                    model_blob,
                    payload.gpu_id,
                    payload.endpoint,
                    payload.notes,
                    reasoning as i64,
                    payload.mode.as_str(),
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
                            notes, reasoning_enabled, mode, created_at, updated_at \
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
                        notes, reasoning_enabled, mode, created_at, updated_at \
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

    /// Patch only the `endpoint` column. Used by the
    /// BackendSupervisor (Phase 12) to write back the
    /// computed-after-spawn endpoint for managed rows without
    /// disturbing the rest of the operator's config.
    pub fn set_endpoint(
        &self,
        purpose: BackendPurpose,
        endpoint: Option<&str>,
        now: i64,
    ) -> Result<(), BackendError> {
        let endpoint_owned = endpoint.map(|s| s.to_owned());
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE config_backends \
                 SET endpoint = ?1, updated_at = ?2 \
                 WHERE purpose = ?3",
                params![endpoint_owned, now, purpose.as_str()],
            )?;
            Ok(())
        })
        .map_err(BackendError::from)
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
    let mode_str: String = row.get(7)?;
    // Unknown mode strings collapse to External as a safe default —
    // refusing to load the row would brick the Backends page on a
    // corrupt enum value.
    let mode = BackendMode::parse(&mode_str).unwrap_or(BackendMode::External);
    Ok(BackendRow {
        purpose,
        inference_backend: row.get(1)?,
        model_spec_json,
        gpu_id: row.get(3)?,
        endpoint: row.get(4)?,
        notes: row.get(5)?,
        reasoning_enabled: row.get::<_, i64>(6)? != 0,
        mode,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
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
            reasoning_enabled: false,
            mode: BackendMode::External,
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
            .upsert(&upsert_payload(BackendPurpose::Small), 100)
            .unwrap();
        let purposes: Vec<_> = store
            .list_all()
            .unwrap()
            .into_iter()
            .map(|r| r.purpose.as_str())
            .collect();
        // Alphabetical by string, which is the SQLite ORDER BY.
        assert_eq!(purposes, vec!["Small", "Standard", "VoiceTTS"]);
    }

    #[test]
    fn clear_removes_one_purpose_only() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        store.upsert(&upsert_payload(BackendPurpose::Standard), 0).unwrap();
        store.upsert(&upsert_payload(BackendPurpose::Small), 0).unwrap();
        assert!(store.clear(BackendPurpose::Standard).unwrap());
        assert!(store.get(BackendPurpose::Standard).unwrap().is_none());
        assert!(store.get(BackendPurpose::Small).unwrap().is_some());
        // Re-clearing returns false.
        assert!(!store.clear(BackendPurpose::Standard).unwrap());
    }

    #[test]
    fn purpose_all_lists_every_enum_value() {
        let names: Vec<_> = BackendPurpose::all().iter().map(|p| p.as_str()).collect();
        assert_eq!(names, vec!["Standard", "Small", "VoiceSTT", "VoiceTTS"]);
    }

    #[test]
    fn reasoning_enabled_only_persists_for_standard() {
        let db = fresh_db();
        let store = BackendStore::new(&db);

        // Standard: reasoning toggle round-trips.
        let mut std_payload = upsert_payload(BackendPurpose::Standard);
        std_payload.reasoning_enabled = true;
        let r = store.upsert(&std_payload, 100).unwrap();
        assert!(r.reasoning_enabled, "Standard preserves reasoning flag");

        // Small: reasoning is silently zeroed regardless of input.
        let mut small_payload = upsert_payload(BackendPurpose::Small);
        small_payload.reasoning_enabled = true;
        let r = store.upsert(&small_payload, 100).unwrap();
        assert!(
            !r.reasoning_enabled,
            "Small must not retain a reasoning flag — it's the fast path",
        );

        // Voice purposes likewise zero the flag.
        let mut stt_payload = upsert_payload(BackendPurpose::VoiceStt);
        stt_payload.reasoning_enabled = true;
        let r = store.upsert(&stt_payload, 100).unwrap();
        assert!(!r.reasoning_enabled);
    }

    #[test]
    fn supports_reasoning_toggle_only_for_standard() {
        assert!(BackendPurpose::Standard.supports_reasoning_toggle());
        for p in [
            BackendPurpose::Small,
            BackendPurpose::VoiceStt,
            BackendPurpose::VoiceTts,
        ] {
            assert!(!p.supports_reasoning_toggle(), "{} must NOT toggle", p.as_str());
        }
    }

    #[test]
    fn parse_round_trips() {
        for p in BackendPurpose::all() {
            assert_eq!(BackendPurpose::parse(p.as_str()), Some(*p));
        }
        assert_eq!(BackendPurpose::parse("nope"), None);
    }

    #[test]
    fn backend_mode_parse_round_trips() {
        for m in [BackendMode::External, BackendMode::Managed] {
            assert_eq!(BackendMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(BackendMode::parse("nope"), None);
    }

    #[test]
    fn upsert_persists_mode_and_round_trips() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        let mut payload = upsert_payload(BackendPurpose::Standard);
        payload.mode = BackendMode::Managed;
        // Managed rows typically don't have an operator-set endpoint.
        // The store doesn't enforce that — it just persists what it
        // gets — but we exercise the realistic shape here.
        payload.endpoint = None;
        let r = store.upsert(&payload, 100).unwrap();
        assert_eq!(r.mode, BackendMode::Managed);
        assert!(r.endpoint.is_none());

        // Re-read; mode survives a round trip and the row continues
        // to honour conflict-update semantics for the rest of the
        // fields.
        let got = store.get(BackendPurpose::Standard).unwrap().unwrap();
        assert_eq!(got.mode, BackendMode::Managed);
    }

    #[test]
    fn fresh_db_rows_default_to_external_mode() {
        // The migration's `DEFAULT 'external'` keeps every legacy
        // row's behaviour identical: nothing turns Managed by
        // surprise. We construct a fresh row via the upsert path
        // (default in the upsert struct is also External) and
        // verify the round-trip.
        let db = fresh_db();
        let store = BackendStore::new(&db);
        let r = store
            .upsert(&upsert_payload(BackendPurpose::Standard), 100)
            .unwrap();
        assert_eq!(r.mode, BackendMode::External);
    }

    #[test]
    fn set_endpoint_only_touches_endpoint_and_updated_at() {
        // The supervisor uses set_endpoint to write back the
        // computed-after-spawn URL. This must not disturb the rest
        // of the operator's config (e.g. mode, model_spec_json).
        let db = fresh_db();
        let store = BackendStore::new(&db);
        let mut payload = upsert_payload(BackendPurpose::Standard);
        payload.mode = BackendMode::Managed;
        payload.endpoint = None;
        store.upsert(&payload, 100).unwrap();

        store
            .set_endpoint(
                BackendPurpose::Standard,
                Some("http://127.0.0.1:8001/v1"),
                200,
            )
            .unwrap();

        let got = store.get(BackendPurpose::Standard).unwrap().unwrap();
        assert_eq!(got.endpoint.as_deref(), Some("http://127.0.0.1:8001/v1"));
        assert_eq!(got.mode, BackendMode::Managed);
        assert_eq!(got.updated_at, 200);
    }

    #[test]
    fn set_endpoint_with_none_clears_the_value() {
        // Used when the supervisor stops a managed backend — the
        // endpoint goes back to "no URL available" so the runner
        // doesn't try to call a dead container.
        let db = fresh_db();
        let store = BackendStore::new(&db);
        store
            .upsert(&upsert_payload(BackendPurpose::Standard), 100)
            .unwrap();
        store
            .set_endpoint(BackendPurpose::Standard, None, 200)
            .unwrap();
        let got = store.get(BackendPurpose::Standard).unwrap().unwrap();
        assert!(got.endpoint.is_none());
    }
}
