//! Routine scheduler tick (Phase 10 + 11.C, MIGRATION_PLAN §5.6.3).
//!
//! Single tokio task that wakes every `TICK_INTERVAL_SECS`, queries
//! routines whose `next_run_at <= now`, and dispatches each as a
//! controller-trust turn via [`crate::chats::dispatch_routine_turn`].
//!
//! Phase 10 shipped this as a stub (every fire marked `Skipped`).
//! Phase 11.C wires the actual dispatch: the routine prompt goes
//! through the same path as a controller-typed message, the
//! conversation id is either the routine's `target_conversation_id`
//! or a freshly-minted one, and the run history row records
//! `Success`/`Failed`/`Skipped` with the resulting conversation id.
//!
//! The tick is wall-clock-aligned: we sleep until the next minute
//! boundary, not a fixed duration from start, so a routine scheduled
//! for `0 * * * *` doesn't slowly skew off the on-the-minute mark.

use crate::events::UiEvent;
use crate::state::AppState;
use chrono::{DurationRound, TimeDelta, TimeZone, Utc};
use execlaw_core::routines::{
    RoutineRunStatus, RoutineStore, next_fire_after, parse_cron, parse_timezone,
};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Wall-clock alignment target: tick at the top of every minute.
const TICK_INTERVAL_SECS: i64 = 60;

/// Spawn the tick task. Owns the state clone for the lifetime of
/// the process; cancellation is dropping the `JoinHandle`.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let inner = Arc::new(Inner { state });
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

            if let Err(e) = inner.tick_once().await {
                warn!("routine scheduler tick failed: {e}");
            }
        }
    })
}

struct Inner {
    state: AppState,
}

impl Inner {
    /// Run one tick: enumerate due routines, dispatch each, advance
    /// `next_run_at`. Errors at the row level are isolated — one
    /// busted routine shouldn't poison the whole tick.
    async fn tick_once(&self) -> Result<(), execlaw_core::routines::RoutineError> {
        let store = RoutineStore::new(&self.state.db);
        let now = Utc::now().timestamp();
        let due = store.list_due(now)?;
        if due.is_empty() {
            return Ok(());
        }
        for routine in due {
            if let Err(e) = self.fire_one(&store, &routine, now).await {
                warn!(
                    "routine fire failed for '{}' ({}): {}",
                    routine.id, routine.name, e
                );
            }
        }
        Ok(())
    }

    async fn fire_one(
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
        self.state.events.publish(UiEvent::RoutineRunChanged {
            routine_id: routine.id.clone(),
            run_id: run_id.clone(),
            status: RoutineRunStatus::Pending.as_str().to_owned(),
        });

        // Phase 11.C — actual dispatch. Routes through the same path
        // as a controller-typed message; falls back to the stub
        // turn when no inference backend is wired so routines still
        // produce success/failure history rows in dev/test
        // environments without a live LLM.
        let outcome = crate::chats::dispatch_routine_turn(
            &self.state,
            &routine.id,
            routine.target_conversation_id.as_deref(),
            &routine.prompt,
        )
        .await;

        let (dispatch_status, dispatch_error, conversation_id) = match outcome {
            Ok(o) => (RoutineRunStatus::Success, None, Some(o.conversation_id)),
            Err(e) => (
                RoutineRunStatus::Failed,
                Some(e),
                routine.target_conversation_id.clone(),
            ),
        };

        store.finish_run(
            &run_id,
            dispatch_status,
            Utc::now().timestamp(),
            dispatch_error.as_deref(),
            conversation_id.as_deref(),
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
        self.state.events.publish(UiEvent::RoutineRunChanged {
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
    use execlaw_core::routines::{RoutineStore, RoutineUpsert};

    fn upsert_one(store: &RoutineStore<'_>, name: &str, cron: &str, now: i64) -> String {
        let row = store
            .upsert(
                &RoutineUpsert {
                    id: None,
                    name: name.into(),
                    schedule_cron: cron.into(),
                    timezone: "UTC".into(),
                    prompt: "do the thing".into(),
                    target_conversation_id: None,
                    enabled: true,
                },
                now,
            )
            .unwrap();
        row.id
    }

    #[tokio::test]
    async fn tick_once_advances_next_run_at_for_every_due_routine() {
        let state = crate::routes::test_app_state();
        let inner = Inner {
            state: state.clone(),
        };
        let store = RoutineStore::new(&state.db);
        let now = Utc::now().timestamp();

        let id = upsert_one(&store, "due", "*/5 * * * *", now);
        store
            .record_run(&id, RoutineRunStatus::Success, now - 600, Some(now - 1))
            .unwrap();

        inner.tick_once().await.unwrap();

        let row = store.get(&id).unwrap().unwrap();
        let next = row.next_run_at.expect("scheduler computed next fire");
        assert!(
            next > now,
            "next_run_at must advance past now (got {next}, now {now})",
        );

        let runs = store.list_runs(&id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_ne!(runs[0].status, RoutineRunStatus::Pending);
    }

    #[tokio::test]
    async fn tick_once_skips_disabled_routines() {
        let state = crate::routes::test_app_state();
        let inner = Inner {
            state: state.clone(),
        };
        let store = RoutineStore::new(&state.db);
        let now = Utc::now().timestamp();

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

        inner.tick_once().await.unwrap();
        let runs = store.list_runs(&row.id, 10).unwrap();
        assert!(runs.is_empty(), "disabled routine must not fire");
    }

    #[tokio::test]
    async fn tick_once_publishes_pending_then_terminal_event() {
        let state = crate::routes::test_app_state();
        let mut rx = state.events.subscribe();
        let inner = Inner {
            state: state.clone(),
        };
        let store = RoutineStore::new(&state.db);
        let now = Utc::now().timestamp();

        let id = upsert_one(&store, "due", "*/5 * * * *", now);
        store
            .record_run(&id, RoutineRunStatus::Success, now - 600, Some(now - 1))
            .unwrap();

        inner.tick_once().await.unwrap();

        let mut pending_run_id: Option<String> = None;
        let mut terminal_seen = false;
        for _ in 0..32 {
            match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
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

    #[tokio::test]
    async fn tick_once_isolates_failure_per_routine() {
        let state = crate::routes::test_app_state();
        let inner = Inner {
            state: state.clone(),
        };
        let store = RoutineStore::new(&state.db);
        let now = Utc::now().timestamp();

        let healthy_id = upsert_one(&store, "ok", "*/5 * * * *", now);
        let broken_id = upsert_one(&store, "broken", "*/5 * * * *", now);
        for id in [&healthy_id, &broken_id] {
            store
                .record_run(id, RoutineRunStatus::Success, now - 600, Some(now - 1))
                .unwrap();
        }

        // Corrupt the broken one's cron AFTER the upsert so the
        // recompute step fails for that row only.
        state
            .db
            .with_conn(|c| {
                c.execute(
                    "UPDATE config_routines SET schedule_cron = 'lol bad' WHERE id = ?1",
                    rusqlite::params![broken_id],
                )?;
                Ok(())
            })
            .unwrap();

        inner.tick_once().await.unwrap();

        let healthy = store.get(&healthy_id).unwrap().unwrap();
        assert!(healthy.next_run_at.unwrap() > now);
    }

    #[tokio::test]
    async fn tick_once_marks_run_success_when_dispatch_lands() {
        // Phase 11.C: with no inference backend wired (test_app_state
        // has inference=None), dispatch_routine_turn falls back to
        // the stub turn — which returns a successful echoed reply.
        // The runner should record Success and capture the
        // freshly-minted conversation_id in the run row.
        let state = crate::routes::test_app_state();
        let inner = Inner {
            state: state.clone(),
        };
        let store = RoutineStore::new(&state.db);
        let now = Utc::now().timestamp();

        let id = upsert_one(&store, "echo", "*/5 * * * *", now);
        store
            .record_run(&id, RoutineRunStatus::Success, now - 600, Some(now - 1))
            .unwrap();

        inner.tick_once().await.unwrap();

        let runs = store.list_runs(&id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].status,
            RoutineRunStatus::Success,
            "stub-turn dispatch should produce a Success run, not Skipped",
        );
        assert!(
            runs[0].conversation_id.is_some(),
            "successful dispatch must record the conversation id it ran on",
        );
        assert!(
            runs[0]
                .conversation_id
                .as_ref()
                .unwrap()
                .starts_with(&format!("routine-{id}-")),
            "auto-minted conversation id should be prefix-tagged with routine id",
        );
    }
}
