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
/// **2026-05-10 — squash to a single baseline.** The first 36
/// migrations were collapsed into one generated `0001_baseline.sql`
/// captured from a fresh DB after applying the historical chain.
/// Pre-v1 + no third-party operators meant there was no upgrade
/// path to preserve; the operator who triggered this accepted the
/// one-time `rm execlaw.db` to land on the baseline cleanly. New
/// schema changes from this point land as additive migrations on
/// top (id 2, 3, …).
///
/// Note: every migration is wrapped in its own transaction by
/// `MigrationRunner::apply_all`.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        id: 1,
        name: "baseline",
        sql: include_str!("../migrations/0001_baseline.sql"),
    },
    // 2026-05-14 — migrations 2-4 (plugin_artifacts, max_history_tokens,
    // principal_identifiers) were folded into the baseline after a
    // checksum-divergence error blocked a single-developer rebuild.
    // The columns + tables those migrations added now live inline in
    // `0001_baseline.sql`; the IDs are retired forever (never reuse).
    Migration {
        id: 5,
        name: "plugin_health",
        sql: include_str!("../migrations/0005_plugin_health.sql"),
    },
    Migration {
        id: 6,
        name: "add_attachments_filename",
        sql: include_str!("../migrations/0006_add_attachments_filename.sql"),
    },
    Migration {
        id: 7,
        name: "automation_bus",
        sql: include_str!("../migrations/0007_automation_bus.sql"),
    },
    Migration {
        id: 8,
        name: "automations",
        sql: include_str!("../migrations/0008_automations.sql"),
    },
    Migration {
        id: 9,
        name: "automation_suggestions",
        sql: include_str!("../migrations/0009_automation_suggestions.sql"),
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

    /// Recompute and overwrite the stored checksum for an
    /// already-applied migration to match the current SQL on disk.
    ///
    /// Use case: `core.autocrlf` (or any other byte-level edit that
    /// doesn't change the SQL semantics — whitespace touch-ups, a
    /// trailing newline added) flips the file's checksum and the
    /// runner refuses to continue with `ChecksumMismatch`. This
    /// command lets the operator confirm "yes, the SQL is the same
    /// to me" and patch the row, without forcing a destructive DB
    /// reset.
    ///
    /// Returns:
    ///   * `Ok(true)`  if the row was patched (or already matched).
    ///   * `Ok(false)` if no row for `id` exists in `schema_version`
    ///     yet — caller should run `apply_all` first.
    ///   * `Err(...)`  if `id` isn't in the embedded migration list.
    ///
    /// This does NOT re-run the SQL. The columns/tables the
    /// migration created stay as they are; only the checksum row
    /// changes. If the SQL has actually diverged in semantically
    /// meaningful ways (a column was renamed, a table dropped),
    /// this is the wrong tool — write a follow-up migration instead.
    pub fn repair_checksum(&self, id: u32) -> Result<bool, MigrationError> {
        let migration = MIGRATIONS.iter().find(|m| m.id == id).ok_or_else(|| {
            MigrationError::Db(DbError::Migration(format!(
                "no embedded migration with id {id}"
            )))
        })?;
        let new_checksum = simple_checksum(migration.sql);
        let updated: usize = self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE schema_version SET checksum = ?1 WHERE id = ?2",
                params![new_checksum, id],
            )?;
            Ok(n)
        })?;
        Ok(updated > 0)
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
///
/// Line endings are normalized to `\n` before hashing. Without this, a
/// migration authored on Windows with CRLF endings produces a different
/// checksum than the same file after `core.autocrlf` round-trips through
/// git — and the runner refuses to continue with a "checksum mismatch"
/// at the next boot, even though the SQL is byte-equivalent semantically.
/// Every other migration in this repo is committed as LF; normalising
/// here makes the runner tolerant of contributors whose checkouts ended
/// up CRLF anyway.
fn simple_checksum(sql: &str) -> String {
    use std::hash::{Hash, Hasher};
    let normalized: String = sql.replace("\r\n", "\n").replace('\r', "\n");
    let mut h = std::collections::hash_map::DefaultHasher::new();
    normalized.hash(&mut h);
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
        // 2026-05-15: migration 5 adds plugin health columns.
        // 2026-05-18: migration 6 adds state_attachments.filename.
        // 2026-05-17: migration 7 adds state_bus_events (M1 of
        // Automations: the durable event-bus substrate).
        // 2026-05-17: migration 8 adds state_automations +
        // state_automation_runs (M2 of Automations: persistent
        // automation defs + run history).
        // 2026-05-17: migration 9 adds state_automation_suggestions
        // + state_automation_muted_patterns (M4 of Automations: the
        // discovery surface on the /automations landing page).
        // Update this list whenever a new migration is added to
        // MIGRATIONS.
        assert_eq!(applied, vec![1, 5, 6, 7, 8, 9]);

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
            "state_research_jobs",
            "config_research",
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
            "state_oauth_clients",
            "state_oauth_tokens",
            "state_oauth_pending",
            "state_skills",
            "state_skill_versions",
            "state_blobs",
            "state_skill_resources",
            "state_skill_invocations",
            "config_skills",
            "state_skill_proposals",
            "state_transport_bindings",
            "memory_promotions",
            "memory_reflections",
            "state_bus_events",
            "state_automations",
            "state_automation_runs",
            "state_automation_suggestions",
            "state_automation_muted_patterns",
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
        assert_eq!(first, vec![1, 5, 6, 7, 8, 9]);
        assert!(
            second.is_empty(),
            "rerun must not re-apply already-applied migrations"
        );
    }

    // ----------------------------------------------------------------
    // 2026-05-10 — squash baseline. The block of "apply through
    // migration N, mutate, run migration N+1, verify repair behaviour"
    // tests was deleted because the migrations they pinned (0019
    // legacy GPU id, 0020 vLLM image bump, 0021 BLOB storage-class
    // repair, 0023 endpoint /v1 suffix, 0036 bind_address default
    // port) no longer exist as separate files. Their post-fix shape
    // is what `0001_baseline.sql` captures, and `apply_all` exercises
    // the baseline on every test below — any regression in the
    // squashed schema surfaces there. If a future migration needs
    // similar "did the repair fire?" coverage, add it alongside that
    // migration.
    // ----------------------------------------------------------------

    // (Historical tests were deleted in the squash. The full content
    // is reachable via `git log` — search for
    // `migration_36_bumps_default_port_only_when_unchanged`,
    // `repair_storage_class_round_trips_blob_correctly`,
    // `vllm_image_bump_targets_only_legacy_v062_managed_rows`,
    // `append_v1_only_touches_bare_loopback_managed_rows`,
    // `legacy_gpu_id_repair_replaces_pnp_strings_and_leaves_clean_values_alone`.)

    /// Regression: a migration file edited from LF to CRLF (e.g. by
    /// `core.autocrlf=true` on a Windows checkout) must hash to the
    /// same checksum as the LF version — otherwise every Windows
    /// contributor whose git config flips line endings would trip
    /// the "already applied with a different checksum" guard on the
    /// next boot, even though the SQL is byte-equivalent.
    /// Operator escape hatch for the CRLF-vs-LF saga: after a file's
    /// checksum drifts (semantic SQL unchanged), `repair_checksum`
    /// writes the recomputed value over the stored row without
    /// re-running the migration body. The columns/tables the
    /// migration created stay put.
    #[test]
    fn repair_checksum_overwrites_stored_value_without_reapplying() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        let runner = MigrationRunner::new(&db);
        runner.apply_all().unwrap();
        // Corrupt the stored checksum for the baseline.
        db.with_conn(|c| {
            c.execute(
                "UPDATE schema_version SET checksum = 'deadbeef' WHERE id = 1",
                [],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
        // A second apply_all must now refuse to continue.
        match runner.apply_all() {
            Err(MigrationError::ChecksumMismatch(1)) => {}
            other => panic!("expected ChecksumMismatch(1), got {other:?}"),
        }
        // Repair fixes it; subsequent apply_all is a no-op.
        let patched = runner.repair_checksum(1).unwrap();
        assert!(patched, "row for id 1 must exist post-apply_all");
        let next = runner.apply_all().unwrap();
        assert!(
            next.is_empty(),
            "no migrations should re-apply after repair"
        );
    }

    #[test]
    fn repair_checksum_returns_false_when_id_not_yet_applied() {
        // Calling repair on a migration that's never been applied
        // is a no-op — there's nothing in schema_version to patch.
        // Return false rather than error so a generic "repair all"
        // script can iterate without special-casing fresh DBs.
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        let runner = MigrationRunner::new(&db);
        // Don't apply migrations — schema_version doesn't even exist
        // yet. Create just the table so the UPDATE has something to
        // run against.
        db.with_conn(|c| {
            c.execute_batch(
                "CREATE TABLE schema_version (\
                    id INTEGER PRIMARY KEY, name TEXT NOT NULL, \
                    checksum TEXT NOT NULL, applied_at INTEGER NOT NULL\
                 );",
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
        let patched = runner.repair_checksum(1).unwrap();
        assert!(!patched);
    }

    #[test]
    fn repair_checksum_unknown_id_errors() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        let runner = MigrationRunner::new(&db);
        runner.apply_all().unwrap();
        let err = runner.repair_checksum(9999).unwrap_err();
        match err {
            MigrationError::Db(DbError::Migration(msg)) => {
                assert!(msg.contains("9999"), "msg: {msg}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn checksum_is_line_ending_tolerant() {
        let lf = "ALTER TABLE foo ADD COLUMN bar TEXT;\nUPDATE foo SET bar = 'x';\n";
        let crlf = "ALTER TABLE foo ADD COLUMN bar TEXT;\r\nUPDATE foo SET bar = 'x';\r\n";
        let cr_only = "ALTER TABLE foo ADD COLUMN bar TEXT;\rUPDATE foo SET bar = 'x';\r";
        assert_eq!(simple_checksum(lf), simple_checksum(crlf));
        assert_eq!(simple_checksum(lf), simple_checksum(cr_only));
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
