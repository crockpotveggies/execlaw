//! Global history-retention policy (migration 0026).
//!
//! One operator-configurable knob in `config_general.history_retention_days`
//! that every history sweeper reads on each tick. Five legal values:
//! `0` (= infinite, never delete) and `30` / `60` / `90` / `120`. The
//! Settings → General API enforces the option set on writes; sweepers
//! defensively round any unknown value down to the nearest legal one
//! so a forgotten future schema bump doesn't disable retention
//! entirely.
//!
//! Subject to retention (consume this module): `state_events`,
//! terminal `state_research_jobs` (forthcoming), `state_routine_runs`,
//! resolved/acked `state_alerts`, structured logs.
//!
//! NOT subject (intentional carve-outs documented in
//! migrations/0026_history_retention.sql): `memory_entries`, audit
//! log, `state_refresh_tokens`, vault rows.
//!
//! 2026-04-29.

use crate::db::{Database, DbError};
use serde::{Deserialize, Serialize};

/// Allowed retention choices, exposed to the Settings UI as a
/// dropdown. New options should be appended; do NOT remove —
/// operators with existing rows expect their stored value to keep
/// resolving cleanly.
pub const ALLOWED_RETENTION_DAYS: &[u32] = &[30, 60, 90, 120];

/// Sentinel meaning "infinite — never delete." Sweepers that see
/// `RetentionPolicy { days: 0 }` skip their tick entirely.
pub const INFINITE_RETENTION: u32 = 0;

/// Default applied when the column is freshly seeded by migration
/// 0026 — operator can immediately change in Settings.
pub const DEFAULT_RETENTION_DAYS: u32 = 30;

/// Resolved retention policy. Cheap to construct (single
/// SELECT against the singleton `config_general` row); sweepers
/// load it once per tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    pub days: u32,
}

impl RetentionPolicy {
    /// Construct from a raw column value. Unknown values are clamped
    /// to the default so a corrupt row can't accidentally disable
    /// retention (which would be a silent operator-data-leak risk).
    pub fn from_days(raw: u32) -> Self {
        if raw == INFINITE_RETENTION
            || ALLOWED_RETENTION_DAYS.contains(&raw)
        {
            Self { days: raw }
        } else {
            Self {
                days: DEFAULT_RETENTION_DAYS,
            }
        }
    }

    /// Load the current policy from the singleton `config_general`
    /// row. Falls back to the default when the column is missing or
    /// unreadable — defensive against migration drift.
    pub fn load(db: &Database) -> Result<Self, DbError> {
        let raw: Option<i64> = db.with_conn(|c| {
            let v: Option<i64> = c
                .query_row(
                    "SELECT history_retention_days FROM config_general WHERE id = 1",
                    [],
                    |r| r.get(0),
                )
                .ok();
            Ok(v)
        })?;
        Ok(match raw {
            Some(n) if n >= 0 => Self::from_days(n as u32),
            _ => Self {
                days: DEFAULT_RETENTION_DAYS,
            },
        })
    }

    /// `None` means "keep forever" — the sweeper should skip this
    /// tick. `Some(cutoff)` is the unix-seconds boundary; rows with
    /// the relevant timestamp column strictly less than this should
    /// be deleted. Clamped at 0 so absurdly long retention windows
    /// against an early epoch produce a no-op rather than a
    /// negative cutoff that no row could ever satisfy.
    pub fn cutoff_for_now(self, now_unix: i64) -> Option<i64> {
        if self.days == INFINITE_RETENTION {
            return None;
        }
        Some(
            now_unix
                .saturating_sub(self.days as i64 * 86_400)
                .max(0),
        )
    }

    /// Same as [`cutoff_for_now`] but in milliseconds — used by the
    /// log-retention sweeper which keys on millisecond timestamps.
    pub fn cutoff_ms_for_now(self, now_ms: i64) -> Option<i64> {
        self.cutoff_for_now(now_ms / 1_000)
            .map(|secs| secs.saturating_mul(1_000))
    }

    pub fn is_infinite(self) -> bool {
        self.days == INFINITE_RETENTION
    }
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            days: DEFAULT_RETENTION_DAYS,
        }
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
    fn default_is_thirty_days() {
        assert_eq!(RetentionPolicy::default().days, DEFAULT_RETENTION_DAYS);
        assert_eq!(DEFAULT_RETENTION_DAYS, 30);
    }

    #[test]
    fn allowed_options_match_settings_ui_contract() {
        assert_eq!(ALLOWED_RETENTION_DAYS, &[30u32, 60, 90, 120]);
    }

    #[test]
    fn from_days_clamps_unknown_values_to_default() {
        // Known good values pass through.
        assert_eq!(RetentionPolicy::from_days(0).days, 0);
        assert_eq!(RetentionPolicy::from_days(30).days, 30);
        assert_eq!(RetentionPolicy::from_days(120).days, 120);
        // Forgotten future option clamps to default rather than
        // accidentally disabling retention.
        assert_eq!(RetentionPolicy::from_days(45).days, 30);
        assert_eq!(RetentionPolicy::from_days(365).days, 30);
        // Negative-equivalent shouldn't reach here (column is i64,
        // we cast as u32 only when >= 0) but defending anyway.
        assert_eq!(RetentionPolicy::from_days(u32::MAX).days, 30);
    }

    #[test]
    fn cutoff_for_finite_policy_is_now_minus_days() {
        let p = RetentionPolicy { days: 30 };
        // Use a `now` well past the retention window so the clamp at
        // zero doesn't fire.
        let now = 1_700_000_000_i64;
        assert_eq!(p.cutoff_for_now(now), Some(now - 30 * 86_400));
    }

    #[test]
    fn cutoff_for_infinite_policy_is_none() {
        let p = RetentionPolicy { days: 0 };
        assert_eq!(p.cutoff_for_now(1_000_000), None);
    }

    #[test]
    fn cutoff_ms_matches_seconds_path() {
        let p = RetentionPolicy { days: 30 };
        let now_ms = 1_700_000_000_000_i64;
        let cutoff_ms = p.cutoff_ms_for_now(now_ms).unwrap();
        assert_eq!(cutoff_ms, (1_700_000_000 - 30 * 86_400) * 1_000);
    }

    #[test]
    fn cutoff_for_now_saturates_at_zero() {
        // now smaller than retention window: cutoff should clamp at
        // 0, not wrap negative.
        let p = RetentionPolicy { days: 1000 };
        assert_eq!(p.cutoff_for_now(100), Some(0));
    }

    #[test]
    fn load_returns_default_on_fresh_db() {
        let db = fresh_db();
        let p = RetentionPolicy::load(&db).unwrap();
        assert_eq!(p.days, DEFAULT_RETENTION_DAYS);
    }

    #[test]
    fn load_reflects_operator_change() {
        let db = fresh_db();
        db.with_conn(|c| {
            c.execute(
                "UPDATE config_general SET history_retention_days = 90 WHERE id = 1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let p = RetentionPolicy::load(&db).unwrap();
        assert_eq!(p.days, 90);
    }

    #[test]
    fn load_clamps_corrupt_value_to_default() {
        let db = fresh_db();
        // Operator (or a migration bug) wrote 45 — not a legal
        // option. Sweeper sees the default instead of skipping
        // retention or using a weird interval.
        db.with_conn(|c| {
            c.execute(
                "UPDATE config_general SET history_retention_days = 45 WHERE id = 1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let p = RetentionPolicy::load(&db).unwrap();
        assert_eq!(p.days, DEFAULT_RETENTION_DAYS);
    }

    #[test]
    fn load_resolves_zero_as_infinite() {
        let db = fresh_db();
        db.with_conn(|c| {
            c.execute(
                "UPDATE config_general SET history_retention_days = 0 WHERE id = 1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let p = RetentionPolicy::load(&db).unwrap();
        assert!(p.is_infinite());
        assert_eq!(p.cutoff_for_now(1_000_000), None);
    }
}
