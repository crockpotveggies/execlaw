//! Hand-rolled migration runner.
//!
//! Per the instructions, we keep this simple: numbered SQL files in
//! `crates/core/migrations/` embedded at compile time via `include_str!`
//! and applied in order, tracked in a `schema_version` table. No `refinery`,
//! no `sqlx-migrate`, no build.rs shenanigans.
//!
//! The file list is intentionally explicit (an array of `(id, name, sql)`
//! tuples) — it's a tiny cost and catches "forgot to register the new
//! migration" at compile time.

use crate::db::{Database, DbError};
use rusqlite::params;
use thiserror::Error;

/// A migration registration: a monotonically increasing ID, a descriptive
/// name (for logs), and the SQL to execute.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub id: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

/// Full list of embedded migrations. Keep sorted by `id`, ascending.
///
/// Note: every migration is wrapped in its own transaction by
/// `MigrationRunner::apply_all`.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        id: 1,
        name: "initial-schema",
        sql: include_str!("../migrations/0001_initial_schema.sql"),
    },
    Migration {
        id: 2,
        name: "event-hmac-tag",
        sql: include_str!("../migrations/0002_event_hmac_tag.sql"),
    },
    Migration {
        id: 3,
        name: "state-plugins",
        sql: include_str!("../migrations/0003_state_plugins.sql"),
    },
    Migration {
        id: 4,
        name: "eval-flagged",
        sql: include_str!("../migrations/0004_eval_flagged.sql"),
    },
    Migration {
        id: 5,
        name: "users",
        sql: include_str!("../migrations/0005_users.sql"),
    },
    Migration {
        id: 6,
        name: "threads-and-transport-conversations",
        sql: include_str!("../migrations/0006_threads_and_transport_conversations.sql"),
    },
    Migration {
        id: 7,
        name: "webauthn-credentials",
        sql: include_str!("../migrations/0007_webauthn_credentials.sql"),
    },
    Migration {
        id: 8,
        name: "refresh-tokens",
        sql: include_str!("../migrations/0008_refresh_tokens.sql"),
    },
    Migration {
        id: 9,
        name: "tool-access",
        sql: include_str!("../migrations/0009_tool_access.sql"),
    },
    Migration {
        id: 10,
        name: "mcp-servers",
        sql: include_str!("../migrations/0010_mcp_servers.sql"),
    },
    Migration {
        id: 11,
        name: "backends",
        sql: include_str!("../migrations/0011_backends.sql"),
    },
    Migration {
        id: 12,
        name: "backend-reshape",
        sql: include_str!("../migrations/0012_backend_reshape.sql"),
    },
    Migration {
        id: 13,
        name: "personality",
        sql: include_str!("../migrations/0013_personality.sql"),
    },
    Migration {
        id: 14,
        name: "routines",
        sql: include_str!("../migrations/0014_routines.sql"),
    },
    Migration {
        id: 15,
        name: "backend-mode",
        sql: include_str!("../migrations/0015_backend_mode.sql"),
    },
    Migration {
        id: 16,
        name: "personality-voice-id-blend",
        sql: include_str!("../migrations/0016_personality_voice_id_blend.sql"),
    },
    Migration {
        id: 17,
        name: "general-settings",
        sql: include_str!("../migrations/0017_general_settings.sql"),
    },
    Migration {
        id: 18,
        name: "general-setup-dismissed",
        sql: include_str!("../migrations/0018_general_setup_dismissed.sql"),
    },
    Migration {
        id: 19,
        name: "repair-legacy-gpu-id",
        sql: include_str!("../migrations/0019_repair_legacy_gpu_id.sql"),
    },
    Migration {
        id: 20,
        name: "bump-vllm-image-to-nightly",
        sql: include_str!("../migrations/0020_bump_vllm_image_to_nightly.sql"),
    },
    Migration {
        id: 21,
        name: "repair-model-spec-storage-class",
        sql: include_str!(
            "../migrations/0021_repair_model_spec_storage_class.sql"
        ),
    },
    Migration {
        id: 22,
        name: "hf-cache",
        sql: include_str!("../migrations/0022_hf_cache.sql"),
    },
    Migration {
        id: 23,
        name: "append-v1-to-managed-endpoints",
        sql: include_str!(
            "../migrations/0023_append_v1_to_managed_endpoints.sql"
        ),
    },
];

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("sqlite error in migration {id} ({name}): {source}")]
    Sqlite {
        id: u32,
        name: &'static str,
        #[source]
        source: rusqlite::Error,
    },
    #[error("migration id {0} already applied but with a different checksum; refusing to continue")]
    ChecksumMismatch(u32),
    #[error("migrations are not monotonic: saw {prev} then {curr}")]
    NotMonotonic { prev: u32, curr: u32 },
}

/// Runs pending migrations against a `Database`.
pub struct MigrationRunner<'a> {
    db: &'a Database,
}

impl<'a> MigrationRunner<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Apply every pending migration in order, inside a transaction each.
    pub fn apply_all(&self) -> Result<Vec<u32>, MigrationError> {
        // Validate monotonicity up front.
        let mut prev = 0u32;
        for m in MIGRATIONS {
            if m.id <= prev {
                return Err(MigrationError::NotMonotonic { prev, curr: m.id });
            }
            prev = m.id;
        }

        // Ensure schema_version table exists.
        self.db.with_conn(|c| {
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_version (\
                    id          INTEGER PRIMARY KEY,\
                    name        TEXT NOT NULL,\
                    checksum    TEXT NOT NULL,\
                    applied_at  INTEGER NOT NULL\
                 );",
            )?;
            Ok(())
        })?;

        let mut applied: Vec<u32> = Vec::new();

        for m in MIGRATIONS {
            let checksum = simple_checksum(m.sql);

            let existing: Option<String> = self.db.with_conn(|c| {
                let got = c
                    .query_row(
                        "SELECT checksum FROM schema_version WHERE id = ?1",
                        params![m.id],
                        |r| r.get::<_, String>(0),
                    )
                    .ok();
                Ok(got)
            })?;

            if let Some(prev_checksum) = existing {
                if prev_checksum != checksum {
                    return Err(MigrationError::ChecksumMismatch(m.id));
                }
                continue;
            }

            // Apply in a transaction.
            self.db
                .transaction(|tx| {
                    tx.execute_batch(m.sql).map_err(|e| {
                        DbError::Migration(format!("migration {} ({}) failed: {e}", m.id, m.name))
                    })?;
                    tx.execute(
                        "INSERT INTO schema_version(id, name, checksum, applied_at) VALUES \
                         (?1, ?2, ?3, strftime('%s','now'))",
                        params![m.id, m.name, checksum],
                    )?;
                    Ok(())
                })
                .map_err(MigrationError::from)?;
            applied.push(m.id);
        }

        Ok(applied)
    }

    /// How many migrations have been applied.
    pub fn applied_count(&self) -> Result<u32, MigrationError> {
        let n: i64 = self.db.with_conn(|c| {
            let v: i64 = c
                .query_row(
                    "SELECT COALESCE(COUNT(*), 0) FROM schema_version",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            Ok(v)
        })?;
        Ok(n as u32)
    }
}

/// Very-not-cryptographic checksum used only to detect in-place edits of an
/// already-applied migration file. sha256 would be overkill; we want `std`-only.
fn simple_checksum(sql: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    sql.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};

    #[test]
    fn apply_all_creates_all_tables() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        let runner = MigrationRunner::new(&db);
        let applied = runner.apply_all().unwrap();
        assert_eq!(
            applied,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23]
        );

        // Spot-check: every documented table exists.
        let tables = vec![
            "state_events",
            "state_conversations",
            "state_outbox",
            "state_inbox",
            "state_alerts",
            "state_incidents",
            "state_alert_silences",
            "state_attachments",
            "state_artifacts",
            "state_plugins",
            "eval_flagged",
            "users",
            "transport_conversations",
            "config_backends",
            "config_trust_policy",
            "config_alert_routing",
            "config_research_quota",
            "config_runtime_settings",
            "config_hardware_profile_overrides",
            "principals",
            "research_jobs",
            "memory_entries",
            "vault_secrets",
            "log_entries",
            "transport_cursors",
            "state_webauthn_credentials",
            "state_refresh_tokens",
            "config_tool_access",
            "config_mcp_servers",
            "config_personality",
            "config_routines",
            "state_routine_runs",
            "config_general",
        ];
        db.with_conn(|c| {
            for t in &tables {
                let count: i64 = c
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                        [t],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                assert_eq!(count, 1, "expected table {} to exist", t);
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn apply_all_is_idempotent() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        let runner = MigrationRunner::new(&db);
        let first = runner.apply_all().unwrap();
        let second = runner.apply_all().unwrap();
        assert_eq!(
            first,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23]
        );
        assert!(
            second.is_empty(),
            "rerun must not re-apply already-applied migrations"
        );
    }

    #[test]
    fn repair_storage_class_round_trips_blob_correctly() {
        // Phase 14 follow-up — migration 0021 fixes rows whose
        // `model_spec_json` storage class drifted from BLOB to TEXT
        // under the buggy first cut of 0020. The repair must:
        //   * leave already-BLOB cells byte-identical
        //   * coerce TEXT cells (UTF-8 JSON) back to BLOB so
        //     `row.get::<_, Vec<u8>>()` succeeds again.
        //
        // We can't easily construct a TEXT cell on a BLOB column via
        // a normal INSERT (rusqlite would store the &[u8] payload as
        // BLOB), so we drive the same UPDATE 0020 ran first to
        // reproduce the storage-class drift, then run 0021 to fix
        // it, then read back via the BackendStore-style `Vec<u8>`
        // pattern.
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        db.with_conn(|c| {
            for m in MIGRATIONS.iter().take_while(|m| m.id < 20) {
                c.execute_batch(m.sql).unwrap();
            }
            c.execute(
                "INSERT INTO config_backends \
                 (purpose, inference_backend, model_spec_json, gpu_id, endpoint, \
                  notes, reasoning_enabled, mode, created_at, updated_at) VALUES \
                 ('Standard', 'service-vllm', \
                   '{\"image\":\"vllm/vllm-openai:v0.6.2\",\"args\":[]}', \
                   '0', NULL, NULL, 0, 'managed', 100, 100)",
                [],
            ).unwrap();
            // Run 0020 (the buggy json_set without CAST). Now
            // model_spec_json is TEXT-affinity.
            let m20 = MIGRATIONS.iter().find(|m| m.id == 20).unwrap();
            c.execute_batch(m20.sql).unwrap();
            Ok(())
        }).unwrap();

        // Verify the storage class IS now TEXT (sanity: this is the
        // bug 0021 fixes).
        let typeof_after_20: String = db
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT typeof(model_spec_json) FROM config_backends \
                     WHERE purpose = 'Standard'",
                    [],
                    |r| r.get(0),
                )
                .unwrap())
            })
            .unwrap();
        assert_eq!(
            typeof_after_20, "text",
            "0020 alone must leave the cell as TEXT; otherwise 0021 has nothing to fix"
        );

        // Run 0021 — repair the storage class.
        db.with_conn(|c| {
            let m21 = MIGRATIONS.iter().find(|m| m.id == 21).unwrap();
            c.execute_batch(m21.sql).unwrap();
            Ok(())
        }).unwrap();

        // Now `typeof()` should be 'blob' and the BackendStore-style
        // read should succeed.
        let (typeof_after_21, blob_bytes): (String, Vec<u8>) = db
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT typeof(model_spec_json), model_spec_json \
                     FROM config_backends WHERE purpose = 'Standard'",
                    [],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Vec<u8>>(1)?,
                        ))
                    },
                )
                .unwrap())
            })
            .unwrap();
        assert_eq!(
            typeof_after_21, "blob",
            "0021 must coerce the TEXT cell back to BLOB"
        );
        // The bytes must still parse as JSON with the bumped image.
        let v: serde_json::Value = serde_json::from_slice(&blob_bytes).unwrap();
        assert_eq!(v["image"], "vllm/vllm-openai:nightly");
    }

    #[test]
    fn vllm_image_bump_targets_only_legacy_v062_managed_rows() {
        // Phase 14 follow-up — migration 0020 rewrites
        // `model_spec_json.image` for managed rows that still hold
        // the locked-in `vllm/vllm-openai:v0.6.2` (pre-Qwen-3.5).
        // It must NOT touch:
        //   * external rows (operator-managed; we don't drive that)
        //   * managed rows with a different image (pinned digests,
        //     custom forks, OpenVINO/OpenArc images)
        //   * rows with no image field at all (malformed but legal)
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        // Apply through migration 19 so config_backends exists.
        db.with_conn(|c| {
            for m in MIGRATIONS.iter().take_while(|m| m.id < 20) {
                c.execute_batch(m.sql).unwrap();
            }
            // Seed four rows representing each "should/shouldn't be
            // rewritten" case.
            c.execute(
                "INSERT INTO config_backends \
                 (purpose, inference_backend, model_spec_json, gpu_id, endpoint, \
                  notes, reasoning_enabled, mode, created_at, updated_at) VALUES \
                 ('Standard', 'service-vllm', \
                   '{\"image\":\"vllm/vllm-openai:v0.6.2\",\"args\":[]}', \
                   '0', NULL, NULL, 0, 'managed', 100, 100), \
                 ('Small',    'service-vllm', \
                   '{\"image\":\"vllm/vllm-openai:v0.6.2\",\"args\":[]}', \
                   '0', NULL, NULL, 0, 'external', 100, 100), \
                 ('VoiceSTT', 'service-whisper-stt', \
                   '{\"image\":\"execlaw/service-whisper-cuda:v1\",\"args\":[]}', \
                   '0', NULL, NULL, 0, 'managed', 100, 100), \
                 ('VoiceTTS', 'service-vllm', \
                   '{\"args\":[]}', \
                   '0', NULL, NULL, 0, 'managed', 100, 100)",
                [],
            ).unwrap();
            Ok(())
        }).unwrap();

        // Run migration 20.
        db.with_conn(|c| {
            let m = MIGRATIONS.iter().find(|m| m.id == 20).unwrap();
            c.execute_batch(m.sql).unwrap();
            Ok(())
        }).unwrap();

        // Inspect each row.
        let by_purpose = db.with_conn(|c| {
            let mut stmt = c
                .prepare(
                    "SELECT purpose, json_extract(model_spec_json, '$.image') \
                     FROM config_backends",
                )
                .unwrap();
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect::<Vec<_>>();
            Ok(rows)
        }).unwrap();
        let lookup: std::collections::HashMap<_, _> = by_purpose.into_iter().collect();
        // Standard (managed + legacy image) → bumped to nightly.
        assert_eq!(
            lookup["Standard"].as_deref(),
            Some("vllm/vllm-openai:nightly"),
            "managed v0.6.2 row must be bumped to nightly"
        );
        // Small (external) → image left alone (we don't touch
        // external rows; the URL is the operator's, not ours).
        assert_eq!(
            lookup["Small"].as_deref(),
            Some("vllm/vllm-openai:v0.6.2"),
            "external rows must not be rewritten"
        );
        // VoiceSTT (different image) → left alone.
        assert_eq!(
            lookup["VoiceSTT"].as_deref(),
            Some("execlaw/service-whisper-cuda:v1"),
        );
        // VoiceTTS (no image field) → still no image field.
        assert!(lookup["VoiceTTS"].is_none());
    }

    #[test]
    fn append_v1_only_touches_bare_loopback_managed_rows() {
        // Phase 14.D regression — supervisor used to write
        // `http://127.0.0.1:8101` to managed rows; the inference
        // client appends `/chat/completions` to that and vLLM 404s
        // because OpenAI routes are mounted under `/v1/...`.
        // Migration 0023 retrofits existing rows.
        //
        // Verify it:
        //   * rewrites bare loopback managed rows  → adds `/v1`
        //   * leaves managed rows that already carry a path
        //   * leaves external rows untouched (operator-typed URL)
        //   * leaves null endpoints alone
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        db.with_conn(|c| {
            for m in MIGRATIONS.iter().take_while(|m| m.id < 23) {
                c.execute_batch(m.sql).unwrap();
            }
            c.execute(
                "INSERT INTO config_backends \
                 (purpose, inference_backend, model_spec_json, gpu_id, endpoint, \
                  notes, reasoning_enabled, mode, created_at, updated_at) VALUES \
                 ('Standard', 'service-vllm', '{}', '0', 'http://127.0.0.1:8101', \
                  NULL, 0, 'managed', 100, 100), \
                 ('Small',    'service-vllm', '{}', '0', 'http://127.0.0.1:8102/v1', \
                  NULL, 0, 'managed', 100, 100), \
                 ('VoiceSTT', 'service-whisper-stt', '{}', '0', 'http://192.168.1.50:8000/v1', \
                  NULL, 0, 'external', 100, 100), \
                 ('VoiceTTS', 'service-piper-tts', '{}', '0', NULL, \
                  NULL, 0, 'managed', 100, 100)",
                [],
            ).unwrap();
            // Apply 0023.
            let m = MIGRATIONS.iter().find(|m| m.id == 23).unwrap();
            c.execute_batch(m.sql).unwrap();
            Ok(())
        }).unwrap();

        let rows = db.with_conn(|c| {
            let mut s = c
                .prepare("SELECT purpose, endpoint FROM config_backends ORDER BY purpose")
                .unwrap();
            let v = s
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })
                .unwrap()
                .map(|x| x.unwrap())
                .collect::<Vec<_>>();
            Ok(v)
        }).unwrap();
        let lookup: std::collections::HashMap<_, _> = rows.into_iter().collect();
        // Standard: bare loopback → suffixed.
        assert_eq!(
            lookup["Standard"].as_deref(),
            Some("http://127.0.0.1:8101/v1")
        );
        // Small: already had /v1 → untouched.
        assert_eq!(
            lookup["Small"].as_deref(),
            Some("http://127.0.0.1:8102/v1")
        );
        // VoiceSTT: external row → untouched even though it
        // matches the "/v1 already present" predicate.
        assert_eq!(
            lookup["VoiceSTT"].as_deref(),
            Some("http://192.168.1.50:8000/v1")
        );
        // VoiceTTS: null endpoint → still null.
        assert_eq!(lookup["VoiceTTS"], None);
    }

    #[test]
    fn legacy_gpu_id_repair_replaces_pnp_strings_and_leaves_clean_values_alone() {
        // Phase 14 follow-up — migration 0019 fixes pre-fix wizard
        // saves where `gpu_id` was the full GpuId string. We seed
        // four rows representing each real shape we've observed +
        // one CUDA UUID (must be left alone) + one already-clean
        // ordinal (must also be left alone) and verify the migration
        // is precise.
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        // Apply through migration 18 first so config_backends exists
        // but migration 19 hasn't run.
        db.with_conn(|c| {
            for m in MIGRATIONS.iter().take_while(|m| m.id < 19) {
                c.execute_batch(m.sql).unwrap();
            }
            // Seed the pre-fix shapes.
            c.execute(
                "INSERT INTO config_backends \
                 (purpose, inference_backend, model_spec_json, gpu_id, endpoint, \
                  notes, reasoning_enabled, mode, created_at, updated_at) VALUES \
                 ('Standard',  'service-vllm', '{}', '0x10de:PCI\\VEN_10DE&DEV_2230&SUBSYS_X', NULL, NULL, 0, 'managed',  100, 100), \
                 ('Small',     'service-vllm', '{}', '0x8086:0xe20b',                          NULL, NULL, 0, 'managed',  100, 100), \
                 ('VoiceSTT',  'service-vllm', '{}', 'GPU-abc-12345',                          NULL, NULL, 0, 'managed',  100, 100), \
                 ('VoiceTTS',  'service-vllm', '{}', '0',                                       NULL, NULL, 0, 'managed',  100, 100)",
                [],
            ).unwrap();
            Ok(())
        }).unwrap();

        // Now run migration 19.
        db.with_conn(|c| {
            let m = MIGRATIONS.iter().find(|m| m.id == 19).unwrap();
            c.execute_batch(m.sql).unwrap();
            Ok(())
        }).unwrap();

        // Inspect every row.
        let by_purpose = db.with_conn(|c| {
            let mut stmt = c
                .prepare("SELECT purpose, gpu_id FROM config_backends")
                .unwrap();
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect::<Vec<_>>();
            Ok(rows)
        }).unwrap();
        let lookup: std::collections::HashMap<_, _> = by_purpose.into_iter().collect();
        // Both legacy shapes get repaired to the per-vendor ordinal "0".
        assert_eq!(lookup["Standard"].as_deref(), Some("0"));
        assert_eq!(lookup["Small"].as_deref(), Some("0"));
        // CUDA UUID is left untouched.
        assert_eq!(lookup["VoiceSTT"].as_deref(), Some("GPU-abc-12345"));
        // Already-clean ordinal is left untouched.
        assert_eq!(lookup["VoiceTTS"].as_deref(), Some("0"));
    }

    #[test]
    fn state_events_has_hmac_tag_column() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db.with_conn(|c| {
            let has_tag: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('state_events') WHERE name = 'tag'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(has_tag, 1, "state_events must have a tag column");
            let has_key: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('state_events') WHERE name = 'key_id'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(has_key, 1, "state_events must have a key_id column");
            Ok(())
        })
        .unwrap();
    }
}
