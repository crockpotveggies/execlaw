//! `RoutineRunRetentionSweeper` — purges `state_routine_runs` rows
//! past the configured retention window (§5.6.1, default 90 days).
//!
//! Mirrors `LogRetentionSweeper` and `EphemeralSweeper` so operators
//! reason about all three with the same mental model: a long-running
//! tokio task that wakes every `interval`, deletes rows older than
//! `now - retention`, and exits cleanly on a stop signal.
//!
//! Pending runs are preserved regardless of age — see
//! `RoutineStore::purge_runs_older_than` for why.

use crate::db::{Database, DbError};
use crate::routines::{RoutineError, RoutineStore};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Default sweep cadence — once an hour. Routine-run rows accrete
/// far slower than `log_entries`, so a sub-hourly cadence isn't
/// useful.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Default retention — 90 days, per MIGRATION_PLAN §5.6.1. Operators
/// who want a longer audit trail can override at construction time.
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(90 * 24 * 60 * 60);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutineRunSweepReport {
    pub rows_deleted: usize,
}

/// Run a single retention pass. Pure-ish — caller supplies `now_unix`
/// so tests pin time deterministically.
pub fn sweep_once(
    db: &Database,
    now_unix: i64,
    retention_secs: i64,
) -> Result<RoutineRunSweepReport, RoutineError> {
    let cutoff = now_unix.saturating_sub(retention_secs);
    let store = RoutineStore::new(db);
    let n = store.purge_runs_older_than(cutoff)?;
    if n > 0 {
        debug!(
            rows = n,
            cutoff_unix = cutoff,
            "routine-run retention sweep"
        );
    }
    Ok(RoutineRunSweepReport { rows_deleted: n })
}

#[derive(Clone)]
pub struct RoutineRunRetentionSweeper {
    db: Database,
    interval: Duration,
    retention: Duration,
    kick: Arc<Notify>,
}

impl RoutineRunRetentionSweeper {
    pub fn new(db: Database) -> Self {
        Self::with_config(db, DEFAULT_SWEEP_INTERVAL, DEFAULT_RETENTION)
    }

    pub fn with_config(
        db: Database,
        interval: Duration,
        retention: Duration,
    ) -> Self {
        Self {
            db,
            interval,
            retention,
            kick: Arc::new(Notify::new()),
        }
    }

    /// Force the run loop to sweep now. Coalesces — repeated kicks
    /// while the loop is busy collapse into one extra sweep.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// Drive the sweep loop until `stop` is notified.
    pub async fn run(&self, stop: Arc<Notify>) {
        info!(
            interval_secs = self.interval.as_secs(),
            retention_secs = self.retention.as_secs(),
            "routine-run retention sweeper running"
        );
        loop {
            let tick = tokio::time::sleep(self.interval);
            tokio::select! {
                _ = tick => {}
                _ = self.kick.notified() => {}
                _ = stop.notified() => {
                    info!("routine-run retention sweeper stop received; draining once and exiting");
                    let _ = self.sweep_now();
                    return;
                }
            }
            if let Err(e) = self.sweep_now() {
                warn!(error = %e, "routine-run retention sweep failed; will retry next tick");
            }
        }
    }

    fn sweep_now(&self) -> Result<RoutineRunSweepReport, DbError> {
        let now_unix = chrono::Utc::now().timestamp();
        sweep_once(&self.db, now_unix, self.retention.as_secs() as i64).map_err(|e| match e {
            RoutineError::Db(db) => db,
            RoutineError::Sqlite(s) => DbError::Sqlite(s),
            // Invalid / NotFound are logically programmer errors at
            // the sweeper layer (the sweeper never validates routine
            // payloads or looks up by id) — collapse into an
            // Invariant so the calling task surfaces them in `warn!`.
            RoutineError::Invalid(msg) => DbError::Invariant(msg),
            RoutineError::NotFound(id) => {
                DbError::Invariant(format!("unexpected NotFound during sweep: {id}"))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;
    use crate::migrations::MigrationRunner;
    use crate::routines::{RoutineRunStatus, RoutineStore, RoutineUpsert};

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn seed_routine(store: &RoutineStore<'_>, name: &str, now: i64) -> String {
        store
            .upsert(
                &RoutineUpsert {
                    id: None,
                    name: name.into(),
                    schedule_cron: "0 8 * * *".into(),
                    timezone: "UTC".into(),
                    prompt: "do".into(),
                    target_conversation_id: None,
                    enabled: true,
                },
                now,
            )
            .unwrap()
            .id
    }

    fn seed_run(
        store: &RoutineStore<'_>,
        routine_id: &str,
        fired_at: i64,
        status: RoutineRunStatus,
    ) -> String {
        let id = store.insert_run_pending(routine_id, fired_at).unwrap();
        // Move out of Pending so retention picks it up.
        if status != RoutineRunStatus::Pending {
            store
                .finish_run(&id, status, fired_at, None, None)
                .unwrap();
        }
        id
    }

    fn count_runs(db: &Database) -> i64 {
        db.with_conn(|c| {
            let v: i64 = c
                .query_row("SELECT COUNT(*) FROM state_routine_runs", [], |r| r.get(0))
                .unwrap();
            Ok(v)
        })
        .unwrap()
    }

    #[test]
    fn sweep_deletes_only_terminal_rows_past_cutoff() {
        let db = fresh_db();
        let now = 1_000_000;
        let store = RoutineStore::new(&db);
        let rid = seed_routine(&store, "r", now);

        seed_run(&store, &rid, 100, RoutineRunStatus::Success);   // very old, terminal
        seed_run(&store, &rid, 200, RoutineRunStatus::Failed);    // very old, terminal
        seed_run(&store, &rid, 300, RoutineRunStatus::Pending);   // very old but PENDING — keep
        seed_run(&store, &rid, 999_999, RoutineRunStatus::Success); // recent, keep

        // retention=500 → cutoff = 999_500. Strictly older than that
        // AND terminal gets deleted.
        let r = sweep_once(&db, now, 500).unwrap();
        assert_eq!(r.rows_deleted, 2);
        assert_eq!(count_runs(&db), 2, "kept Pending and the recent Success");
    }

    #[test]
    fn sweep_is_idempotent() {
        let db = fresh_db();
        let store = RoutineStore::new(&db);
        let rid = seed_routine(&store, "r", 1_000);
        seed_run(&store, &rid, 100, RoutineRunStatus::Success);
        let _ = sweep_once(&db, 10_000, 1_000).unwrap();
        let again = sweep_once(&db, 10_000, 1_000).unwrap();
        assert_eq!(again.rows_deleted, 0);
    }

    #[test]
    fn sweep_handles_now_smaller_than_retention_via_saturating_sub() {
        let db = fresh_db();
        let store = RoutineStore::new(&db);
        let rid = seed_routine(&store, "r", 1_000);
        seed_run(&store, &rid, 0, RoutineRunStatus::Success);
        // saturating_sub keeps cutoff at 0; nothing strictly less.
        let r = sweep_once(&db, 10, 1_000_000_000).unwrap();
        assert_eq!(r.rows_deleted, 0);
        assert_eq!(count_runs(&db), 1);
    }

    #[tokio::test]
    async fn run_loop_sweeps_then_stops_on_signal() {
        let db = fresh_db();
        let store = RoutineStore::new(&db);
        let rid = seed_routine(&store, "r", 1_000_000);
        // Old terminal run. Wall-clock retention of 1s means the
        // tick eats it.
        seed_run(&store, &rid, 1, RoutineRunStatus::Success);

        let sweeper = RoutineRunRetentionSweeper::with_config(
            db.clone(),
            Duration::from_millis(20),
            Duration::from_secs(1),
        );
        let stop = Arc::new(Notify::new());
        let stop_clone = stop.clone();
        let sweeper_clone = sweeper.clone();
        let handle = tokio::spawn(async move { sweeper_clone.run(stop_clone).await });

        sweeper.kick();
        tokio::time::sleep(Duration::from_millis(80)).await;
        stop.notify_one();
        handle.await.unwrap();

        assert_eq!(count_runs(&db), 0);
    }
}
