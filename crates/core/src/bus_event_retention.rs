//! `BusEventRetentionSweeper` — purges dispatched `state_bus_events`
//! rows past the global `history_retention_days` window.
//!
//! Mirrors `LogRetentionSweeper`, `EventRetentionSweeper`, and
//! `RoutineRunRetentionSweeper` in shape: a long-running tokio task
//! that wakes every `interval`, reads the operator-configured
//! [`crate::retention::RetentionPolicy`] from the DB, deletes
//! dispatched events older than `now - retention`, and exits cleanly
//! on a stop signal.
//!
//! Critical invariant carried over from `BusEventStore::purge_dispatched_older_than`:
//! **pending rows (rows with `dispatched_at IS NULL`) are NEVER
//! swept**, regardless of age. Retention must not paper over a stuck
//! dispatcher.
//!
//! 2026-05-17 (M1 of Automations).

use crate::automation_bus::{BusEventError, BusEventStore};
use crate::db::Database;
use crate::retention::RetentionPolicy;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Default sweep cadence — once every two hours. Matches
/// `EventRetentionSweeper` so the two history sweepers share a
/// schedule and operators see consistent disk-reclaim timing.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);

/// One-pass sweep. Pure-ish: caller supplies `now_unix` so tests can
/// pin time deterministically.
pub fn sweep_once(
    db: &Database,
    now_unix: i64,
    retention_secs: i64,
) -> Result<usize, BusEventError> {
    let cutoff = now_unix.saturating_sub(retention_secs).max(0);
    let n = BusEventStore::new(db).purge_dispatched_older_than(cutoff)?;
    if n > 0 {
        debug!(
            rows = n,
            cutoff_unix = cutoff,
            "bus event retention sweep"
        );
    }
    Ok(n)
}

/// Long-running sweeper actor. Constructed in `cmd_serve`; runs for
/// the lifetime of the process.
#[derive(Clone)]
pub struct BusEventRetentionSweeper {
    db: Database,
    interval: Duration,
    /// `Some(d)` pins retention regardless of operator policy
    /// (tests). `None` means "load [`RetentionPolicy`] on each tick"
    /// — the production path. Mirrors `EventRetentionSweeper`.
    static_retention: Option<Duration>,
    kick: Arc<Notify>,
}

impl BusEventRetentionSweeper {
    pub fn new(db: Database) -> Self {
        Self {
            db,
            interval: DEFAULT_SWEEP_INTERVAL,
            static_retention: None,
            kick: Arc::new(Notify::new()),
        }
    }

    pub fn with_config(db: Database, interval: Duration, retention: Duration) -> Self {
        Self {
            db,
            interval,
            static_retention: Some(retention),
            kick: Arc::new(Notify::new()),
        }
    }

    pub fn kick(&self) {
        self.kick.notify_one();
    }

    pub async fn run(&self, stop: Arc<Notify>) {
        info!(
            interval_secs = self.interval.as_secs(),
            retention = match self.static_retention {
                Some(d) => format!("static:{}s", d.as_secs()),
                None => "policy".into(),
            },
            "bus-event-retention sweeper running",
        );
        loop {
            let tick = tokio::time::sleep(self.interval);
            tokio::select! {
                _ = tick => {}
                _ = self.kick.notified() => {}
                _ = stop.notified() => {
                    info!("bus-event-retention stop received; draining once and exiting");
                    let _ = self.sweep_now();
                    return;
                }
            }
            if let Err(e) = self.sweep_now() {
                warn!(error = %e, "bus-event-retention sweep failed; retry next tick");
            }
        }
    }

    fn sweep_now(&self) -> Result<usize, BusEventError> {
        let now_unix = chrono::Utc::now().timestamp();
        let retention_secs = match self.static_retention {
            Some(d) => d.as_secs() as i64,
            None => {
                let policy = RetentionPolicy::load(&self.db)?;
                if policy.is_infinite() {
                    return Ok(0);
                }
                policy.days as i64 * 86_400
            }
        };
        sweep_once(&self.db, now_unix, retention_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation_bus::{BusEventKind, BusEventStore, Event};
    use crate::db::DbConfig;
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn evt(id: &str, ts: i64) -> Event {
        Event {
            id: id.into(),
            kind: BusEventKind::WebhookReceived,
            source: "test".into(),
            received_at: ts,
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn sweep_deletes_old_dispatched_rows_only() {
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        store.publish(&evt("old-done", 100), false).unwrap();
        store.publish(&evt("old-pending", 100), false).unwrap();
        store.publish(&evt("recent-done", 999_999), false).unwrap();
        assert!(store.mark_dispatched("old-done", 100).unwrap());
        assert!(store.mark_dispatched("recent-done", 999_999).unwrap());
        // retention=500 → cutoff=999_500. Old (100) gets swept;
        // pending (any age) is preserved; recent (999_999) stays.
        let n = sweep_once(&db, 1_000_000, 500).unwrap();
        assert_eq!(n, 1);
        assert!(store.get("old-done").unwrap().is_none());
        assert!(store.get("old-pending").unwrap().is_some());
        assert!(store.get("recent-done").unwrap().is_some());
    }

    #[test]
    fn sweep_now_skips_when_policy_is_infinite() {
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        store.publish(&evt("old-done", 100), false).unwrap();
        assert!(store.mark_dispatched("old-done", 100).unwrap());
        db.with_conn(|c| {
            c.execute(
                "UPDATE config_general SET history_retention_days = 0 WHERE id = 1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let sweeper = BusEventRetentionSweeper::new(db.clone());
        let n = sweeper.sweep_now().unwrap();
        assert_eq!(n, 0);
        assert!(store.get("old-done").unwrap().is_some());
    }

    #[test]
    fn sweep_is_idempotent() {
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        store.publish(&evt("e", 100), false).unwrap();
        assert!(store.mark_dispatched("e", 100).unwrap());
        let first = sweep_once(&db, 1_000_000, 500).unwrap();
        let second = sweep_once(&db, 1_000_000, 500).unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 0);
    }

    #[tokio::test]
    async fn run_loop_drains_on_stop_signal() {
        let db = fresh_db();
        let store = BusEventStore::new(&db);
        store.publish(&evt("e", 1), false).unwrap();
        assert!(store.mark_dispatched("e", 1).unwrap());
        let sweeper = BusEventRetentionSweeper::with_config(
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
        assert!(BusEventStore::new(&db).get("e").unwrap().is_none());
    }
}
