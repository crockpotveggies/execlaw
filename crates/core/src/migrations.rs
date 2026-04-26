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
        assert_eq!(applied, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);

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
        assert_eq!(first, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        assert!(
            second.is_empty(),
            "rerun must not re-apply already-applied migrations"
        );
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
