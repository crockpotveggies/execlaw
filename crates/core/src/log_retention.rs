//! `LogRetentionSweeper` — purges old `log_entries` rows past their
//! retention window (Phase 7 §7-Hardening: "log retention + privacy
//! controls").
//!
//! 2026-04-29 — retention is now driven by the global Settings →
//! General `history_retention_days` choice (`crate::retention::
//! RetentionPolicy`), not a hardcoded 30 days. The sweeper loads the
//! policy on each tick so a Settings change takes effect within one
//! sweep cadence. `Infinite` (operator picked "never delete") makes
//! the sweep a no-op for the tick. Tests still pin retention via
//! `with_config` for determinism.

use crate::db::{Database, DbError};
use crate::logs::LogStore;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Default sweep cadence.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Default retention window — 30 days. Used only by `with_config`
/// fixtures that explicitly want the historical default; production
/// uses the operator-configured `history_retention_days` value.
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Outcome of one sweep pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogSweepReport {
    pub rows_deleted: usize,
}

/// Run a single retention pass: deletes every row whose `ts_ms` is
/// strictly older than `now_ms - retention_ms`. Pure-ish — caller
/// supplies `now_ms` so tests can pin time deterministically.
pub fn sweep_once(
    db: &Database,
    now_ms: i64,
    retention_ms: i64,
) -> Result<LogSweepReport, DbError> {
    let cutoff = now_ms.saturating_sub(retention_ms);
    let store = LogStore::new(db);
    let n = store.purge_older_than(cutoff)?;
    if n > 0 {
        debug!(rows = n, cutoff_ms = cutoff, "log retention sweep");
    }
    Ok(LogSweepReport { rows_deleted: n })
}

/// Long-running sweeper task.
#[derive(Clone)]
pub struct LogRetentionSweeper {
    db: Database,
    interval: Duration,
    /// `Some(d)` pins retention regardless of operator policy
    /// (tests + the legacy `with_config` constructor). `None` means
    /// "load the live policy on each tick" — the production path,
    /// so Settings changes take effect within one sweep cadence.
    static_retention: Option<Duration>,
    kick: Arc<Notify>,
}

impl LogRetentionSweeper {
    /// Production constructor: read retention live from the operator's
    /// `history_retention_days` setting on every tick.
    pub fn new(db: Database) -> Self {
        Self {
            db,
            interval: DEFAULT_SWEEP_INTERVAL,
            static_retention: None,
            kick: Arc::new(Notify::new()),
        }
    }

    /// Pin a static retention duration (tests + fixtures that want
    /// deterministic behaviour without consulting the DB row).
    pub fn with_config(db: Database, interval: Duration, retention: Duration) -> Self {
        Self {
            db,
            interval,
            static_retention: Some(retention),
            kick: Arc::new(Notify::new()),
        }
    }

    /// Force the run loop to sweep now. Coalesces — multiple kicks
    /// while the loop is busy collapse into one extra sweep.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// Drive the sweep loop until `stop` is notified. Reads `now`
    /// from the system clock per tick.
    pub async fn run(&self, stop: Arc<Notify>) {
        info!(
            interval_secs = self.interval.as_secs(),
            retention = match self.static_retention {
                Some(d) => format!("static:{}s", d.as_secs()),
                None => "policy".into(),
            },
            "log retention sweeper running"
        );
        loop {
            let tick = tokio::time::sleep(self.interval);
            tokio::select! {
                _ = tick => {}
                _ = self.kick.notified() => {}
                _ = stop.notified() => {
                    info!("log retention sweeper stop received; draining once and exiting");
                    let _ = self.sweep_now();
                    return;
                }
            }
            if let Err(e) = self.sweep_now() {
                warn!(error = %e, "log retention sweep failed; will retry next tick");
            }
        }
    }

    fn sweep_now(&self) -> Result<LogSweepReport, DbError> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let retention_ms = match self.static_retention {
            Some(d) => d.as_millis() as i64,
            None => {
                let policy = crate::retention::RetentionPolicy::load(&self.db)?;
                if policy.is_infinite() {
                    // Operator chose "never delete" — no-op tick.
                    return Ok(LogSweepReport::default());
                }
                policy.days as i64 * 86_400 * 1_000
            }
        };
        sweep_once(&self.db, now_ms, retention_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;
    use crate::logs::{LogLevel, LogRow, LogStore};
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn seed(db: &Database, ts_ms: i64) {
        LogStore::new(db)
            .insert(&LogRow {
                ts_ms,
                level: LogLevel::Info,
                target: "t".into(),
                conversation_id: None,
                plugin_id: None,
                message: format!("@{ts_ms}"),
                fields_json: None,
            })
            .unwrap();
    }

    fn count(db: &Database) -> i64 {
        db.with_conn(|c| {
            let v: i64 = c
                .query_row("SELECT COUNT(*) FROM log_entries", [], |r| r.get(0))
                .unwrap();
            Ok(v)
        })
        .unwrap()
    }

    #[test]
    fn sweep_deletes_only_rows_past_retention() {
        let db = fresh_db();
        seed(&db, 100);
        seed(&db, 200);
        seed(&db, 300);
        // now=400, retention=150 → cutoff=250. Drops 100, 200.
        let r = sweep_once(&db, 400, 150).unwrap();
        assert_eq!(r.rows_deleted, 2);
        assert_eq!(count(&db), 1);
    }

    #[test]
    fn sweep_is_idempotent_on_repeat() {
        let db = fresh_db();
        seed(&db, 50);
        seed(&db, 60);
        let _ = sweep_once(&db, 1_000, 100).unwrap();
        // Second call removes nothing.
        let r = sweep_once(&db, 1_000, 100).unwrap();
        assert_eq!(r.rows_deleted, 0);
    }

    #[test]
    fn sweep_with_zero_retention_drops_every_old_row() {
        let db = fresh_db();
        for ts in [10, 20, 30] {
            seed(&db, ts);
        }
        // now=100, retention=0 → cutoff=100, strictly less drops everything.
        let r = sweep_once(&db, 100, 0).unwrap();
        assert_eq!(r.rows_deleted, 3);
        assert_eq!(count(&db), 0);
    }

    #[test]
    fn sweep_handles_now_smaller_than_retention_via_saturating_sub() {
        let db = fresh_db();
        seed(&db, 0);
        // saturating_sub keeps the cutoff at 0; nothing strictly less.
        let r = sweep_once(&db, 10, 1_000_000).unwrap();
        assert_eq!(r.rows_deleted, 0);
        assert_eq!(count(&db), 1);
    }

    #[tokio::test]
    async fn run_loop_sweeps_then_stops_on_signal() {
        let db = fresh_db();
        // One row well in the past relative to wall-clock now.
        seed(&db, 1);

        let sweeper = LogRetentionSweeper::with_config(
            db.clone(),
            Duration::from_millis(20),
            Duration::from_millis(50),
        );
        let stop = Arc::new(Notify::new());
        let stop_clone = stop.clone();
        let sweeper_clone = sweeper.clone();
        let handle = tokio::spawn(async move { sweeper_clone.run(stop_clone).await });

        sweeper.kick();
        tokio::time::sleep(Duration::from_millis(80)).await;
        stop.notify_one();
        handle.await.unwrap();

        // The seeded row is older than 50ms; sweep should have removed it.
        assert_eq!(count(&db), 0);
    }
}
