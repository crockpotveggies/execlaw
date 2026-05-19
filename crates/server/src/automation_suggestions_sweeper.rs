//! Daily sweeper that populates `state_automation_suggestions` (M4).
//!
//! Mirrors the shape of `EventRetentionSweeper` /
//! `BusEventRetentionSweeper`: a long-running tokio actor that wakes
//! every `interval`, calls `SuggestionStore::sweep`, and exits
//! cleanly on a stop signal.
//!
//! Cadence: once per day by default (24h). Suggestions are a
//! discovery surface, not an alerting one — daily freshness is
//! plenty, and a slower cadence keeps the sweep out of the way
//! of more time-sensitive workers.

use execlaw_core::Database;
use execlaw_core::automation_suggestions::{
    DEFAULT_SWEEP_INTERVAL_SECS, SuggestionError, SuggestionStore,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

#[derive(Clone)]
pub struct AutomationSuggestionsSweeper {
    db: Database,
    interval: Duration,
    kick: Arc<Notify>,
}

impl AutomationSuggestionsSweeper {
    pub fn new(db: Database) -> Self {
        Self {
            db,
            interval: Duration::from_secs(DEFAULT_SWEEP_INTERVAL_SECS),
            kick: Arc::new(Notify::new()),
        }
    }

    pub fn with_interval(db: Database, interval: Duration) -> Self {
        Self {
            db,
            interval,
            kick: Arc::new(Notify::new()),
        }
    }

    /// Wake the sweeper outside its normal cadence — handy on first
    /// boot so the landing page has fresh suggestions without waiting
    /// 24h, and exercised in tests.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    pub async fn run(&self, stop: Arc<Notify>) {
        info!(
            interval_secs = self.interval.as_secs(),
            "automation suggestions sweeper running",
        );
        // Initial-kick fires shortly after boot so the operator sees
        // suggestions on first visit to /automations rather than
        // after a 24h wait. Subsequent ticks follow `interval`.
        let first_delay = Duration::from_secs(60);
        let mut delay = first_delay;
        loop {
            let tick = tokio::time::sleep(delay);
            tokio::select! {
                _ = tick => {}
                _ = self.kick.notified() => {}
                _ = stop.notified() => {
                    info!("automation suggestions sweeper: stop received; final sweep then exit");
                    let _ = self.sweep_now();
                    return;
                }
            }
            if let Err(e) = self.sweep_now() {
                warn!(error = %e, "automation suggestions sweep failed; retry next tick");
            }
            delay = self.interval;
        }
    }

    fn sweep_now(&self) -> Result<usize, SuggestionError> {
        let now = chrono::Utc::now().timestamp();
        let store = SuggestionStore::new(&self.db);
        let n = store.sweep(now)?;
        if n > 0 {
            debug!(rows = n, "automation suggestions sweep produced rows");
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::automation_bus::{BusEventKind, BusEventStore, Event as BusEvent};
    use execlaw_core::db::DbConfig;
    use execlaw_core::migrations::MigrationRunner;
    use std::time::Duration;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn seed(db: &Database, source: &str, n: i64) {
        let bus = BusEventStore::new(db);
        let now_ms = chrono::Utc::now().timestamp_millis();
        for i in 0..n {
            bus.publish(
                &BusEvent {
                    id: format!("{source}-{i}"),
                    kind: BusEventKind::WebhookReceived,
                    source: source.into(),
                    received_at: now_ms + i,
                    payload: serde_json::json!({}),
                },
                false,
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn kick_produces_a_sweep_run() {
        let db = fresh_db();
        seed(&db, "webhook:ring", 15);
        // Long interval — the test relies on `kick` to trigger.
        let sweeper =
            AutomationSuggestionsSweeper::with_interval(db.clone(), Duration::from_secs(60 * 60));
        let stop = Arc::new(Notify::new());
        let stop_clone = stop.clone();
        let sweeper_clone = sweeper.clone();
        let handle = tokio::spawn(async move { sweeper_clone.run(stop_clone).await });
        // Race window: the run() loop is asleep on the 60s first_delay.
        // kick() notifies; the next tokio::select! wakes on `kick.notified()`.
        sweeper.kick();
        // Wait long enough for the sweep to run.
        tokio::time::sleep(Duration::from_millis(150)).await;
        stop.notify_one();
        handle.await.unwrap();
        let store = SuggestionStore::new(&db);
        assert_eq!(store.list_pending().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stop_drains_a_final_sweep() {
        let db = fresh_db();
        seed(&db, "webhook:final", 15);
        let sweeper =
            AutomationSuggestionsSweeper::with_interval(db.clone(), Duration::from_secs(60 * 60));
        let stop = Arc::new(Notify::new());
        let stop_clone = stop.clone();
        let sweeper_clone = sweeper.clone();
        let handle = tokio::spawn(async move { sweeper_clone.run(stop_clone).await });
        // Don't kick — go straight to stop. The select! arm for stop
        // does a final sweep on its way out.
        tokio::time::sleep(Duration::from_millis(20)).await;
        stop.notify_one();
        handle.await.unwrap();
        let store = SuggestionStore::new(&db);
        assert_eq!(store.list_pending().unwrap().len(), 1);
    }
}
