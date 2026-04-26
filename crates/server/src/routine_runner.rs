//! Routine scheduler tick (Phase 10, MIGRATION_PLAN §5.6.3).
//!
//! Single tokio task that wakes every `TICK_INTERVAL_SECS`, queries
//! routines whose `next_run_at <= now`, and dispatches each as a
//! controller turn. The dispatch path itself is a stub for v1 — we
//! insert the run-history row, mark it `Skipped` with an explanatory
//! error, and advance `next_run_at`. When `runner-local` is real this
//! is where the prompt → turn handoff happens.
//!
//! The tick is wall-clock-aligned: we sleep until the next minute
//! boundary, not a fixed duration from start, so a routine scheduled
//! for `0 * * * *` doesn't slowly skew off the on-the-minute mark.

use crate::events::{EventBus, UiEvent};
use chrono::{DurationRound, TimeDelta, TimeZone, Utc};
use execlaw_core::routines::{
    next_fire_after, parse_cron, parse_timezone, RoutineRunStatus, RoutineStore,
};
use execlaw_core::Database;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Wall-clock alignment target: tick at the top of every minute.
const TICK_INTERVAL_SECS: i64 = 60;

/// Spawn the tick task. Owns no resources beyond the `Database`
/// clone + a handle to the event bus, so cancellation is just
/// dropping the returned `JoinHandle`.
pub fn spawn(db: Database, events: EventBus) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let inner = Arc::new(Inner { db, events });
        info!(
            "routine scheduler running; interval_secs={}",
            TICK_INTERVAL_SECS
        );
        loop {
            // Sleep until the next minute boundary so the cron-shaped
            // schedules see fires aligned to the wall clock.
            let now = Utc::now();
            let next_minute = match now.duration_round(TimeDelta::seconds(TICK_INTERVAL_SECS)) {
                Ok(t) if t > now => t,
                Ok(t) => t + TimeDelta::seconds(TICK_INTERVAL_SECS),
                Err(_) => now + TimeDelta::seconds(TICK_INTERVAL_SECS),
            };
            let sleep_for = (next_minute - now)
                .to_std()
                .unwrap_or(Duration::from_secs(TICK_INTERVAL_SECS as u64));
            tokio::time::sleep(sleep_for).await;

            if let Err(e) = inner.tick_once() {
                warn!("routine scheduler tick failed: {e}");
            }
        }
    })
}

struct Inner {
    db: Database,
    events: EventBus,
}

impl Inner {
    /// Run one tick: enumerate due routines, dispatch each, advance
    /// `next_run_at`. Errors at the row level are isolated — one
    /// busted routine shouldn't poison the whole tick.
    fn tick_once(&self) -> Result<(), execlaw_core::routines::RoutineError> {
        let store = RoutineStore::new(&self.db);
        let now = Utc::now().timestamp();
        let due = store.list_due(now)?;
        if due.is_empty() {
            return Ok(());
        }
        for routine in due {
            if let Err(e) = self.fire_one(&store, &routine, now) {
                warn!(
                    "routine fire failed for '{}' ({}): {}",
                    routine.id, routine.name, e
                );
            }
        }
        Ok(())
    }

    fn fire_one(
        &self,
        store: &RoutineStore<'_>,
        routine: &execlaw_core::routines::RoutineRow,
        now: i64,
    ) -> Result<(), execlaw_core::routines::RoutineError> {
        // Insert a Pending run row first so the operator sees the
        // attempt even if dispatch fails. Publish a Pending event
        // immediately so the SPA's run-history drawer reflects the
        // attempt without polling.
        let run_id = store.insert_run_pending(&routine.id, now)?;
        self.events.publish(UiEvent::RoutineRunChanged {
            routine_id: routine.id.clone(),
            run_id: run_id.clone(),
            status: RoutineRunStatus::Pending.as_str().to_owned(),
        });

        // v1 dispatch is a stub: mark the run Skipped with an
        // explanatory error so the operator can see "yes, the schedule
        // fired, but the dispatch pipeline isn't wired yet." Once
        // runner-local is real, this is where we hand off to the
        // turn executor.
        let dispatch_status = RoutineRunStatus::Skipped;
        let dispatch_error = Some(
            "scheduler fired; turn dispatch lands with runner-local",
        );
        store.finish_run(
            &run_id,
            dispatch_status,
            now,
            dispatch_error,
            routine.target_conversation_id.as_deref(),
        )?;

        // Recompute next_run_at so the routine doesn't fire again on
        // this same minute. A schedule whose next fire we can't compute
        // is rolled to None — the operator sees "next: never" and can
        // fix the cron.
        let next_run_at = match (
            parse_cron(&routine.schedule_cron),
            parse_timezone(&routine.timezone),
        ) {
            (Ok(sched), Ok(tz)) => {
                let after = Utc.timestamp_opt(now, 0).single().unwrap_or_else(Utc::now);
                next_fire_after(&sched, tz, after).map(|t| t.timestamp())
            }
            _ => None,
        };
        store.record_run(&routine.id, dispatch_status, now, next_run_at)?;

        // Notify the SPA so the run history view updates live without
        // waiting for a refresh.
        self.events.publish(UiEvent::RoutineRunChanged {
            routine_id: routine.id.clone(),
            run_id,
            status: dispatch_status.as_str().to_owned(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::DbConfig;
    use execlaw_core::routines::{RoutineUpsert, RoutineStore};
    use execlaw_core::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn upsert_one(store: &RoutineStore<'_>, name: &str, cron: &str, now: i64) -> String {
        let row = store
            .upsert(
                &RoutineUpsert {
                    id: None,
                    name: name.into(),
                    schedule_cron: cron.into(),
                    timezone: "UTC".into(),
                    prompt: "do".into(),
                    target_conversation_id: None,
                    enabled: true,
                },
                now,
            )
            .unwrap();
        row.id
    }

    #[test]
    fn tick_once_advances_next_run_at_for_every_due_routine() {
        let db = fresh_db();
        let bus = EventBus::new();
        let inner = Inner { db: db.clone(), events: bus };
        let store = RoutineStore::new(&db);
        let now = Utc::now().timestamp();

        // Force a routine into "due" by recording a past fire.
        let id = upsert_one(&store, "due", "*/5 * * * *", now);
        store
            .record_run(&id, RoutineRunStatus::Success, now - 600, Some(now - 1))
            .unwrap();

        inner.tick_once().unwrap();

        // After tick, next_run_at must have advanced past `now`.
        let row = store.get(&id).unwrap().unwrap();
        let next = row.next_run_at.expect("scheduler computed next fire");
        assert!(
            next > now,
            "next_run_at must advance past now (got {next}, now {now})",
        );

        // A run history row exists and is in a terminal status.
        let runs = store.list_runs(&id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_ne!(runs[0].status, RoutineRunStatus::Pending);
    }

    #[test]
    fn tick_once_skips_disabled_routines() {
        let db = fresh_db();
        let bus = EventBus::new();
        let inner = Inner { db: db.clone(), events: bus };
        let store = RoutineStore::new(&db);
        let now = Utc::now().timestamp();

        // Create an enabled routine, then disable + force "due".
        let row = store
            .upsert(
                &RoutineUpsert {
                    id: None,
                    name: "off".into(),
                    schedule_cron: "*/5 * * * *".into(),
                    timezone: "UTC".into(),
                    prompt: "do".into(),
                    target_conversation_id: None,
                    enabled: false,
                },
                now,
            )
            .unwrap();
        store
            .record_run(&row.id, RoutineRunStatus::Success, now - 600, Some(now - 1))
            .unwrap();

        inner.tick_once().unwrap();
        let runs = store.list_runs(&row.id, 10).unwrap();
        assert!(runs.is_empty(), "disabled routine must not fire");
    }

    #[tokio::test]
    async fn tick_once_publishes_pending_then_terminal_event() {
        let db = fresh_db();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let inner = Inner { db: db.clone(), events: bus };
        let store = RoutineStore::new(&db);
        let now = Utc::now().timestamp();

        let id = upsert_one(&store, "due", "*/5 * * * *", now);
        store
            .record_run(&id, RoutineRunStatus::Success, now - 600, Some(now - 1))
            .unwrap();

        inner.tick_once().unwrap();

        // Drain the channel and assert: a Pending event arrives
        // before its terminal counterpart for the same run_id.
        let mut pending_run_id: Option<String> = None;
        let mut terminal_seen = false;
        for _ in 0..16 {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                rx.recv(),
            )
            .await
            {
                Ok(Ok(UiEvent::RoutineRunChanged {
                    routine_id,
                    run_id,
                    status,
                })) if routine_id == id => {
                    if status == "Pending" {
                        pending_run_id = Some(run_id);
                    } else if pending_run_id.as_deref() == Some(run_id.as_str()) {
                        terminal_seen = true;
                        break;
                    }
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(
            pending_run_id.is_some(),
            "scheduler must publish a Pending RoutineRunChanged",
        );
        assert!(
            terminal_seen,
            "scheduler must publish a terminal RoutineRunChanged for the same run_id",
        );
    }

    #[test]
    fn tick_once_isolates_failure_per_routine() {
        // Two routines; we manually corrupt one's cron to make
        // record_run's recompute fail. The other should still fire.
        let db = fresh_db();
        let bus = EventBus::new();
        let inner = Inner { db: db.clone(), events: bus };
        let store = RoutineStore::new(&db);
        let now = Utc::now().timestamp();

        let healthy_id = upsert_one(&store, "ok", "*/5 * * * *", now);
        let broken_id = upsert_one(&store, "broken", "*/5 * * * *", now);

        // Force both due.
        for id in [&healthy_id, &broken_id] {
            store
                .record_run(id, RoutineRunStatus::Success, now - 600, Some(now - 1))
                .unwrap();
        }

        // Corrupt the broken one's cron AFTER the upsert.
        db.with_conn(|c| {
            c.execute(
                "UPDATE config_routines SET schedule_cron = 'lol bad' WHERE id = ?1",
                rusqlite::params![broken_id],
            )?;
            Ok(())
        })
        .unwrap();

        // tick_once must NOT propagate the per-routine failure.
        inner.tick_once().unwrap();

        // Healthy advanced.
        let healthy = store.get(&healthy_id).unwrap().unwrap();
        assert!(healthy.next_run_at.unwrap() > now);
    }
}
