//! Routines (cron-shaped agent automations) — §5.6.
//!
//! Operator-authored cron schedules paired with prompts the agent
//! runs on its own. The store is the durability layer; the
//! `RoutineRunner` (in the server crate) ticks every minute, picks
//! the routines whose `next_run_at` has elapsed, and dispatches the
//! prompt as a controller turn.
//!
//! Cron parsing uses the `cron` crate. We restrict to standard
//! 5-field syntax (`minute hour day-of-month month day-of-week`) at
//! the validation layer for predictable operator UX, even though the
//! crate accepts 6- and 7-field forms. The operator's chosen IANA
//! timezone is used for evaluation; `next_run_at` is stored as Unix
//! UTC so the scheduler tick is timezone-agnostic.

use crate::db::{Database, DbError};
use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutineRunStatus {
    Pending,
    Success,
    Failed,
    Skipped,
}

impl RoutineRunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Success => "Success",
            Self::Failed => "Failed",
            Self::Skipped => "Skipped",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Pending" => Some(Self::Pending),
            "Success" => Some(Self::Success),
            "Failed" => Some(Self::Failed),
            "Skipped" => Some(Self::Skipped),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutineRow {
    pub id: String,
    pub name: String,
    pub schedule_cron: String,
    pub timezone: String,
    pub prompt: String,
    pub target_conversation_id: Option<String>,
    pub enabled: bool,
    pub last_run_at: Option<i64>,
    pub last_run_status: Option<RoutineRunStatus>,
    pub next_run_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct RoutineUpsert {
    /// `None` on create (id is minted server-side), `Some` on update.
    pub id: Option<String>,
    pub name: String,
    pub schedule_cron: String,
    pub timezone: String,
    pub prompt: String,
    pub target_conversation_id: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutineRunRow {
    pub id: String,
    pub routine_id: String,
    pub fired_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub status: RoutineRunStatus,
    pub error: Option<String>,
    pub conversation_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum RoutineError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("rusqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid routine: {0}")]
    Invalid(String),
    #[error("routine not found: {0}")]
    NotFound(String),
}

/// Parse a cron expression in the standard 5-field form. Reject
/// 6- and 7-field forms even though the underlying crate accepts
/// them, so the operator UX is predictable.
pub fn parse_cron(expr: &str) -> Result<Schedule, RoutineError> {
    let trimmed = expr.trim();
    let field_count = trimmed.split_whitespace().count();
    if field_count != 5 {
        return Err(RoutineError::Invalid(format!(
            "cron expression must have 5 fields (minute hour day-of-month month day-of-week), got {field_count}"
        )));
    }
    // The `cron` crate expects 6- or 7-field syntax (it requires a
    // seconds field). Prepend `0` so 5-field operator input parses
    // as "fire at second 0 of the minute."
    let promoted = format!("0 {trimmed}");
    Schedule::from_str(&promoted)
        .map_err(|e| RoutineError::Invalid(format!("cron parse failed: {e}")))
}

/// Validate an IANA tz string. Returns the parsed `Tz` so callers
/// can plug it straight into the cron evaluator.
pub fn parse_timezone(tz: &str) -> Result<Tz, RoutineError> {
    Tz::from_str(tz).map_err(|e| RoutineError::Invalid(format!("invalid timezone '{tz}': {e}")))
}

/// Compute the next fire time strictly after `after` for the given
/// schedule, evaluated in `tz`. Returns `None` if the schedule has
/// no upcoming occurrence in the next ~10 years (catches typos like
/// `0 0 30 2 *` that would never fire).
pub fn next_fire_after(
    schedule: &Schedule,
    tz: Tz,
    after: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let after_local = after.with_timezone(&tz);
    schedule
        .after(&after_local)
        .next()
        .map(|t| t.with_timezone(&Utc))
}

/// Compute the next N fire times for the SPA preview pane.
pub fn next_n_fires(
    schedule: &Schedule,
    tz: Tz,
    after: DateTime<Utc>,
    n: usize,
) -> Vec<DateTime<Utc>> {
    let after_local = after.with_timezone(&tz);
    schedule
        .after(&after_local)
        .take(n)
        .map(|t| t.with_timezone(&Utc))
        .collect()
}

pub struct RoutineStore<'db> {
    db: &'db Database,
}

impl<'db> RoutineStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Insert or update a routine. On insert the caller must NOT
    /// pre-populate `id`; the store mints a UUID and writes it back
    /// in the returned row. `next_run_at` is computed from the
    /// schedule + tz; failure to compute it is NOT a hard error
    /// (a routine with no upcoming run is still saveable, just
    /// dormant).
    pub fn upsert(
        &self,
        payload: &RoutineUpsert,
        now: i64,
    ) -> Result<RoutineRow, RoutineError> {
        if payload.name.trim().is_empty() {
            return Err(RoutineError::Invalid(
                "routine name is required".into(),
            ));
        }
        let schedule = parse_cron(&payload.schedule_cron)?;
        let tz = parse_timezone(&payload.timezone)?;
        let after_dt = Utc.timestamp_opt(now, 0).single().ok_or_else(|| {
            RoutineError::Invalid(format!("invalid `now` timestamp: {now}"))
        })?;
        let next_run_at = next_fire_after(&schedule, tz, after_dt).map(|t| t.timestamp());

        // Reject schedules whose next fire is more than ~1 year out
        // (catches `0 0 1 1 *` typos that would mean "once per year").
        if let Some(nra) = next_run_at {
            if nra - now > 366 * 24 * 60 * 60 {
                return Err(RoutineError::Invalid(format!(
                    "schedule's next fire is over a year out (cron '{}' tz '{}'); reject as likely typo",
                    payload.schedule_cron, payload.timezone
                )));
            }
        }

        let id = payload
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let enabled_int: i64 = if payload.enabled { 1 } else { 0 };

        // Ownership-style copies so the closure captures by value.
        let id_for_query = id.clone();
        let name = payload.name.clone();
        let schedule_cron = payload.schedule_cron.clone();
        let tz_str = payload.timezone.clone();
        let prompt = payload.prompt.clone();
        let target = payload.target_conversation_id.clone();

        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO config_routines \
                   (id, name, schedule_cron, timezone, prompt, \
                    target_conversation_id, enabled, last_run_at, \
                    last_run_status, next_run_at, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, ?8, ?9, ?9) \
                 ON CONFLICT(id) DO UPDATE SET \
                    name                  = excluded.name, \
                    schedule_cron         = excluded.schedule_cron, \
                    timezone              = excluded.timezone, \
                    prompt                = excluded.prompt, \
                    target_conversation_id= excluded.target_conversation_id, \
                    enabled               = excluded.enabled, \
                    next_run_at           = excluded.next_run_at, \
                    updated_at            = excluded.updated_at",
                params![
                    id_for_query,
                    name,
                    schedule_cron,
                    tz_str,
                    prompt,
                    target,
                    enabled_int,
                    next_run_at,
                    now,
                ],
            )?;
            Ok(())
        })?;

        self.get(&id)?
            .ok_or(RoutineError::NotFound(id))
    }

    pub fn get(&self, id: &str) -> Result<Option<RoutineRow>, RoutineError> {
        let id_owned = id.to_owned();
        self.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT id, name, schedule_cron, timezone, prompt, \
                            target_conversation_id, enabled, last_run_at, \
                            last_run_status, next_run_at, created_at, updated_at \
                     FROM config_routines WHERE id = ?1",
                    params![id_owned],
                    row_to_routine,
                )
                .ok();
            Ok(got)
        })
        .map_err(RoutineError::from)
    }

    /// List every routine, ordered by enabled-first then by next fire
    /// (soonest at the top). Disabled routines sink below.
    pub fn list_all(&self) -> Result<Vec<RoutineRow>, RoutineError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, schedule_cron, timezone, prompt, \
                        target_conversation_id, enabled, last_run_at, \
                        last_run_status, next_run_at, created_at, updated_at \
                 FROM config_routines \
                 ORDER BY enabled DESC, \
                          CASE WHEN next_run_at IS NULL THEN 1 ELSE 0 END, \
                          next_run_at ASC, \
                          name ASC",
            )?;
            let rows = stmt
                .query_map([], row_to_routine)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows)
    }

    pub fn delete(&self, id: &str) -> Result<bool, RoutineError> {
        let id_owned = id.to_owned();
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM config_routines WHERE id = ?1",
                params![id_owned],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }

    /// Routines whose `next_run_at <= now` and `enabled = 1`. The
    /// scheduler tick uses this to find what to fire.
    pub fn list_due(&self, now: i64) -> Result<Vec<RoutineRow>, RoutineError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, schedule_cron, timezone, prompt, \
                        target_conversation_id, enabled, last_run_at, \
                        last_run_status, next_run_at, created_at, updated_at \
                 FROM config_routines \
                 WHERE enabled = 1 AND next_run_at IS NOT NULL AND next_run_at <= ?1 \
                 ORDER BY next_run_at ASC",
            )?;
            let rows = stmt
                .query_map(params![now], row_to_routine)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows)
    }

    /// Mark a fire as completed and recompute next_run_at. Single
    /// atomic statement so two scheduler ticks racing on the same
    /// routine can't double-advance.
    pub fn record_run(
        &self,
        id: &str,
        status: RoutineRunStatus,
        last_run_at: i64,
        next_run_at: Option<i64>,
    ) -> Result<(), RoutineError> {
        let id_owned = id.to_owned();
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE config_routines \
                 SET last_run_at = ?1, last_run_status = ?2, \
                     next_run_at = ?3, updated_at = ?1 \
                 WHERE id = ?4",
                params![last_run_at, status.as_str(), next_run_at, id_owned],
            )?;
            Ok(())
        })
        .map_err(RoutineError::from)
    }

    /// Insert a new run-history row in `Pending` status. Returns the
    /// minted run id.
    pub fn insert_run_pending(
        &self,
        routine_id: &str,
        fired_at: i64,
    ) -> Result<String, RoutineError> {
        let run_id = uuid::Uuid::new_v4().to_string();
        let routine_id_owned = routine_id.to_owned();
        let run_id_for_query = run_id.clone();
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_routine_runs \
                   (id, routine_id, fired_at, started_at, finished_at, \
                    status, error, conversation_id) \
                 VALUES (?1, ?2, ?3, NULL, NULL, 'Pending', NULL, NULL)",
                params![run_id_for_query, routine_id_owned, fired_at],
            )?;
            Ok(())
        })?;
        Ok(run_id)
    }

    /// Move a run from Pending → terminal status. The runner calls
    /// this when the dispatched turn finishes (or fails).
    pub fn finish_run(
        &self,
        run_id: &str,
        status: RoutineRunStatus,
        finished_at: i64,
        error: Option<&str>,
        conversation_id: Option<&str>,
    ) -> Result<(), RoutineError> {
        let id_owned = run_id.to_owned();
        let err_owned = error.map(|s| s.to_owned());
        let cid_owned = conversation_id.map(|s| s.to_owned());
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE state_routine_runs \
                 SET status = ?1, finished_at = ?2, error = ?3, conversation_id = ?4 \
                 WHERE id = ?5",
                params![status.as_str(), finished_at, err_owned, cid_owned, id_owned],
            )?;
            Ok(())
        })
        .map_err(RoutineError::from)
    }

    /// Run history for a routine, most-recent first. Capped to
    /// `limit` so a deep history doesn't dump a million rows.
    pub fn list_runs(
        &self,
        routine_id: &str,
        limit: u32,
    ) -> Result<Vec<RoutineRunRow>, RoutineError> {
        let routine_id_owned = routine_id.to_owned();
        let lim = limit.min(500) as i64;
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, routine_id, fired_at, started_at, finished_at, \
                        status, error, conversation_id \
                 FROM state_routine_runs \
                 WHERE routine_id = ?1 \
                 ORDER BY fired_at DESC \
                 LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![routine_id_owned, lim], row_to_run)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows)
    }
}

fn row_to_routine(row: &rusqlite::Row<'_>) -> rusqlite::Result<RoutineRow> {
    let status_str: Option<String> = row.get(8)?;
    let last_run_status = status_str.and_then(|s| RoutineRunStatus::parse(&s));
    let enabled: i64 = row.get(6)?;
    Ok(RoutineRow {
        id: row.get(0)?,
        name: row.get(1)?,
        schedule_cron: row.get(2)?,
        timezone: row.get(3)?,
        prompt: row.get(4)?,
        target_conversation_id: row.get(5)?,
        enabled: enabled != 0,
        last_run_at: row.get(7)?,
        last_run_status,
        next_run_at: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RoutineRunRow> {
    let status_str: String = row.get(5)?;
    let status = RoutineRunStatus::parse(&status_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown routine run status: {status_str}"),
            )),
        )
    })?;
    Ok(RoutineRunRow {
        id: row.get(0)?,
        routine_id: row.get(1)?,
        fired_at: row.get(2)?,
        started_at: row.get(3)?,
        finished_at: row.get(4)?,
        status,
        error: row.get(6)?,
        conversation_id: row.get(7)?,
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

    fn upsert(name: &str, cron: &str) -> RoutineUpsert {
        RoutineUpsert {
            id: None,
            name: name.into(),
            schedule_cron: cron.into(),
            timezone: "UTC".into(),
            prompt: "do the thing".into(),
            target_conversation_id: None,
            enabled: true,
        }
    }

    #[test]
    fn parse_cron_accepts_standard_5_field() {
        assert!(parse_cron("0 8 * * 1-5").is_ok());
        assert!(parse_cron("*/15 * * * *").is_ok());
        assert!(parse_cron("0 0 1 1 *").is_ok());
    }

    #[test]
    fn parse_cron_rejects_non_5_field_forms() {
        // 6-field with seconds — legal in `cron` crate but rejected here.
        assert!(parse_cron("0 0 8 * * 1-5").is_err());
        // 4-field — short.
        assert!(parse_cron("0 8 * *").is_err());
        // Empty.
        assert!(parse_cron("").is_err());
        // Garbage.
        assert!(parse_cron("not a cron").is_err());
    }

    #[test]
    fn next_fire_after_respects_timezone() {
        let sched = parse_cron("0 8 * * *").unwrap();
        // 2026-04-25 03:00 UTC → next 8am NY (= 12:00 UTC) the same day.
        let after = Utc.with_ymd_and_hms(2026, 4, 25, 3, 0, 0).unwrap();
        let tz = parse_timezone("America/New_York").unwrap();
        let next = next_fire_after(&sched, tz, after).expect("has a fire");
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 4, 25, 12, 0, 0).unwrap());
    }

    #[test]
    fn upsert_creates_then_updates_with_next_run_at() {
        let db = fresh_db();
        let store = RoutineStore::new(&db);
        let now = Utc.with_ymd_and_hms(2026, 4, 25, 3, 0, 0).unwrap().timestamp();
        let r1 = store.upsert(&upsert("morning", "0 8 * * *"), now).unwrap();
        assert!(!r1.id.is_empty());
        assert!(r1.next_run_at.is_some());

        // Update via id round-trip.
        let mut p = upsert("morning v2", "0 9 * * *");
        p.id = Some(r1.id.clone());
        let r2 = store.upsert(&p, now).unwrap();
        assert_eq!(r2.id, r1.id);
        assert_eq!(r2.name, "morning v2");
        // The schedule changed, so next_run_at should differ.
        assert_ne!(r1.next_run_at, r2.next_run_at);
    }

    #[test]
    fn upsert_rejects_schedule_that_fires_more_than_a_year_out() {
        let db = fresh_db();
        let store = RoutineStore::new(&db);
        // April 25 2026 → next Feb 29 = Feb 29 2028 = ~22 months out.
        // That's the typo-catching case: legitimate "every leap-day"
        // schedules are vanishingly rare, so the guardrail prefers
        // rejecting on the assumption it's a fat-fingered cron.
        let now = Utc.with_ymd_and_hms(2026, 4, 25, 3, 0, 0).unwrap().timestamp();
        let err = store
            .upsert(&upsert("leap-only", "0 0 29 2 *"), now)
            .unwrap_err();
        assert!(matches!(err, RoutineError::Invalid(_)));
    }

    #[test]
    fn upsert_accepts_yearly_schedule_when_next_fire_is_within_a_year() {
        let db = fresh_db();
        let store = RoutineStore::new(&db);
        // Dec 1 2026 → next Jan 1 = Jan 1 2027 = ~31 days out → fine.
        let now = Utc.with_ymd_and_hms(2026, 12, 1, 3, 0, 0).unwrap().timestamp();
        let r = store
            .upsert(&upsert("yearly-but-soon", "0 0 1 1 *"), now)
            .unwrap();
        assert!(r.next_run_at.is_some());
    }

    #[test]
    fn upsert_rejects_invalid_cron_and_tz_and_empty_name() {
        let db = fresh_db();
        let store = RoutineStore::new(&db);
        let now = 0;
        assert!(store.upsert(&upsert("bad-cron", "lol"), now).is_err());

        let mut p = upsert("bad-tz", "0 8 * * *");
        p.timezone = "Mars/Olympus_Mons".into();
        assert!(store.upsert(&p, now).is_err());

        let mut p = upsert("", "0 8 * * *");
        p.name = "".into();
        assert!(store.upsert(&p, now).is_err());
    }

    #[test]
    fn list_due_returns_only_enabled_with_elapsed_next_run_at() {
        let db = fresh_db();
        let store = RoutineStore::new(&db);
        let now = Utc.with_ymd_and_hms(2026, 4, 25, 3, 0, 0).unwrap().timestamp();

        let due = store.upsert(&upsert("due", "0 8 * * *"), now).unwrap();
        // Force its next_run_at into the past.
        store
            .record_run(&due.id, RoutineRunStatus::Success, now - 600, Some(now - 1))
            .unwrap();

        // Disabled routine, also overdue — should NOT appear.
        let mut disabled_p = upsert("disabled", "0 8 * * *");
        disabled_p.enabled = false;
        let disabled = store.upsert(&disabled_p, now).unwrap();
        store
            .record_run(
                &disabled.id,
                RoutineRunStatus::Success,
                now - 600,
                Some(now - 1),
            )
            .unwrap();

        let due_rows = store.list_due(now).unwrap();
        assert_eq!(due_rows.len(), 1);
        assert_eq!(due_rows[0].id, due.id);
    }

    #[test]
    fn run_history_round_trip() {
        let db = fresh_db();
        let store = RoutineStore::new(&db);
        let now = Utc.with_ymd_and_hms(2026, 4, 25, 3, 0, 0).unwrap().timestamp();
        let r = store.upsert(&upsert("test", "0 8 * * *"), now).unwrap();

        let run_id = store.insert_run_pending(&r.id, now).unwrap();
        store
            .finish_run(
                &run_id,
                RoutineRunStatus::Success,
                now + 5,
                None,
                Some("conv-123"),
            )
            .unwrap();

        let runs = store.list_runs(&r.id, 50).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RoutineRunStatus::Success);
        assert_eq!(runs[0].finished_at, Some(now + 5));
        assert_eq!(runs[0].conversation_id.as_deref(), Some("conv-123"));
    }

    #[test]
    fn delete_then_list_runs_is_empty_via_fk_cascade() {
        let db = fresh_db();
        let store = RoutineStore::new(&db);
        let now = Utc.with_ymd_and_hms(2026, 4, 25, 3, 0, 0).unwrap().timestamp();
        let r = store.upsert(&upsert("test", "0 8 * * *"), now).unwrap();
        store.insert_run_pending(&r.id, now).unwrap();

        assert!(store.delete(&r.id).unwrap());
        let runs = store.list_runs(&r.id, 50).unwrap();
        assert!(runs.is_empty(), "cascade should drop the run history");
    }

    #[test]
    fn next_n_fires_returns_n_distinct_future_times() {
        let sched = parse_cron("0 8 * * *").unwrap();
        let tz = parse_timezone("UTC").unwrap();
        let after = Utc.with_ymd_and_hms(2026, 4, 25, 3, 0, 0).unwrap();
        let fires = next_n_fires(&sched, tz, after, 3);
        assert_eq!(fires.len(), 3);
        // Each is strictly after the previous.
        assert!(fires[0] < fires[1]);
        assert!(fires[1] < fires[2]);
    }
}
