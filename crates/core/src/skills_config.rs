//! Phase C — singleton-row store for the skill subsystem's
//! operator-editable settings.
//!
//! Mirrors [`crate::general_settings::GeneralSettingsStore`]:
//! one row per DB (PRIMARY KEY 1, CHECK enforced in the schema),
//! `INSERT OR IGNORE` seeds defaults, the table is migrated by
//! `0030_skills_config.sql`.
//!
//! Used by the auto-capture worker to decide whether to summarize
//! a completed turn into a draft skill, and by the operator UI
//! (Settings → Skills) to flip the toggle.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// Operator-editable skill-subsystem settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsConfig {
    /// When `false` (the default), the auto-capture worker is
    /// dormant — turn completions are observed but no draft skills
    /// are produced. Operator opts in via the UI.
    pub auto_capture_enabled: bool,
    /// Threshold below which a turn does not qualify for
    /// summarization. Default 5.
    pub auto_capture_min_tool_calls: u32,
    /// When `true`, the worker runs the full pipeline
    /// (replay + sanitize + summarize) but does NOT write a skill
    /// row directly — instead the proposal lands in
    /// `state_skill_proposals` for operator review (Phase D.1).
    pub auto_capture_dry_run: bool,
    /// Phase D.3 — when `true`, the reuse-update worker observes
    /// completed skill invocations and produces version-fork
    /// proposals when an outcome reveals an improvement. Always
    /// produces a proposal for review, never mutates `stable`
    /// directly.
    pub reuse_update_enabled: bool,
    pub updated_at: i64,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            auto_capture_enabled: false,
            auto_capture_min_tool_calls: 5,
            auto_capture_dry_run: false,
            reuse_update_enabled: false,
            updated_at: 0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillsConfigUpdate {
    pub auto_capture_enabled: Option<bool>,
    pub auto_capture_min_tool_calls: Option<u32>,
    pub auto_capture_dry_run: Option<bool>,
    pub reuse_update_enabled: Option<bool>,
}

pub struct SkillsConfigStore<'a> {
    db: &'a Database,
}

impl<'a> SkillsConfigStore<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Read the singleton row. Returns the seeded default
    /// (`auto_capture_enabled = false`) when the row is missing —
    /// migration 0030 always seeds it, so the fallback is defensive
    /// against migration drift.
    pub fn get(&self) -> Result<SkillsConfig, DbError> {
        let row: Option<(i64, i64, i64, i64, i64)> = self.db.with_conn(|c| {
            Ok(c.query_row(
                "SELECT auto_capture_enabled, auto_capture_min_tool_calls,
                        auto_capture_dry_run, reuse_update_enabled, updated_at
                 FROM config_skills WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .ok())
        })?;
        let Some((enabled, min_calls, dry_run, reuse_update, updated_at)) = row else {
            return Ok(SkillsConfig::default());
        };
        Ok(SkillsConfig {
            auto_capture_enabled: enabled != 0,
            auto_capture_min_tool_calls: min_calls.max(1) as u32,
            auto_capture_dry_run: dry_run != 0,
            reuse_update_enabled: reuse_update != 0,
            updated_at,
        })
    }

    /// Apply a partial update. Fields left `None` keep their current
    /// value. `now_ms` updates `updated_at` regardless of which
    /// fields changed so cache layers can detect any change.
    pub fn update(&self, u: &SkillsConfigUpdate, now_ms: i64) -> Result<SkillsConfig, DbError> {
        if let Some(n) = u.auto_capture_min_tool_calls {
            if !(1..=100).contains(&n) {
                return Err(DbError::Invariant(format!(
                    "auto_capture_min_tool_calls out of range 1..=100: {n}"
                )));
            }
        }
        self.db.transaction(|tx| {
            // Read current to fill in untouched fields.
            let (cur_enabled, cur_min, cur_dry, cur_reuse): (i64, i64, i64, i64) = tx.query_row(
                "SELECT auto_capture_enabled, auto_capture_min_tool_calls,
                        auto_capture_dry_run, reuse_update_enabled
                 FROM config_skills WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            let new_enabled = u
                .auto_capture_enabled
                .map(|b| if b { 1i64 } else { 0 })
                .unwrap_or(cur_enabled);
            let new_min = u
                .auto_capture_min_tool_calls
                .map(|n| n as i64)
                .unwrap_or(cur_min);
            let new_dry = u
                .auto_capture_dry_run
                .map(|b| if b { 1i64 } else { 0 })
                .unwrap_or(cur_dry);
            let new_reuse = u
                .reuse_update_enabled
                .map(|b| if b { 1i64 } else { 0 })
                .unwrap_or(cur_reuse);
            tx.execute(
                "UPDATE config_skills
                 SET auto_capture_enabled = ?1,
                     auto_capture_min_tool_calls = ?2,
                     auto_capture_dry_run = ?3,
                     reuse_update_enabled = ?4,
                     updated_at = ?5
                 WHERE id = 1",
                params![new_enabled, new_min, new_dry, new_reuse, now_ms],
            )?;
            Ok(())
        })?;
        self.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;
    use crate::migrations::MigrationRunner;

    fn fresh() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    #[test]
    fn default_values_match_locked_design() {
        let db = fresh();
        let s = SkillsConfigStore::new(&db).get().unwrap();
        assert!(!s.auto_capture_enabled);
        assert_eq!(s.auto_capture_min_tool_calls, 5);
        assert!(!s.auto_capture_dry_run);
    }

    #[test]
    fn partial_update_keeps_unchanged_fields() {
        let db = fresh();
        let store = SkillsConfigStore::new(&db);
        store
            .update(
                &SkillsConfigUpdate {
                    auto_capture_enabled: Some(true),
                    ..Default::default()
                },
                1000,
            )
            .unwrap();
        let s = store.get().unwrap();
        assert!(s.auto_capture_enabled);
        assert_eq!(s.auto_capture_min_tool_calls, 5);
        assert!(!s.auto_capture_dry_run);
        assert_eq!(s.updated_at, 1000);
    }

    #[test]
    fn out_of_range_threshold_is_rejected() {
        let db = fresh();
        let store = SkillsConfigStore::new(&db);
        let err = store
            .update(
                &SkillsConfigUpdate {
                    auto_capture_min_tool_calls: Some(0),
                    ..Default::default()
                },
                1,
            )
            .unwrap_err();
        assert!(matches!(err, DbError::Invariant(_)));
        let err = store
            .update(
                &SkillsConfigUpdate {
                    auto_capture_min_tool_calls: Some(101),
                    ..Default::default()
                },
                2,
            )
            .unwrap_err();
        assert!(matches!(err, DbError::Invariant(_)));
    }

    #[test]
    fn dry_run_toggle_isolated_from_enabled_toggle() {
        let db = fresh();
        let store = SkillsConfigStore::new(&db);
        store
            .update(
                &SkillsConfigUpdate {
                    auto_capture_dry_run: Some(true),
                    ..Default::default()
                },
                1,
            )
            .unwrap();
        let s = store.get().unwrap();
        assert!(s.auto_capture_dry_run);
        assert!(!s.auto_capture_enabled);
    }
}
