//! In-memory registry of live runners (Phase 8.5).
//!
//! Today's runners are in-process (one logical runner per
//! conversation, materialised on every turn). The container-manager
//! crate will eventually own real per-conversation containers; until
//! then, this registry just tracks the state every turn passes
//! through:
//!
//!   * `register_turn_start(conversation_id, principal_label, modality)`
//!     called when a turn enters `run_tool_capable_turn` or
//!     `run_real_turn`.
//!   * `register_turn_end(conversation_id)` called on the success
//!     or failure path of the same turn.
//!   * `restart(conversation_id)` evicts the entry and (when the
//!     container-manager work lands) sends `SIGTERM` to the
//!     conversation's runner so the next turn rehydrates fresh from
//!     the event log.
//!
//! Lifecycle policy (see `docs/runner-design.md`):
//!   * Controller's pinned thread → **always hot**, never reaped on
//!     idle.
//!   * Every other group → **10-minute idle reap** (configurable
//!     via `IDLE_TTL`).
//!
//! The `idle_reaper` task scrubs entries past their TTL on a 60s
//! cadence. This is a no-op until container-managed runners exist;
//! today the in-process turn lifecycle already returns memory to
//! the pool when the request handler returns, so the registry is
//! purely informational for the Settings → Runners page.

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{debug, info};

/// How long a non-controller runner stays registered after its last
/// activity before the reaper drops it. Mirrors selfhosted-claw's
/// `HOT_RUNNER_POOL_IDLE_TTL_MS = 5 min` but bumped to 10 to match
/// the user's spec ("controller always hot, others 10 min").
pub const IDLE_TTL: Duration = Duration::from_secs(10 * 60);

/// How often the reaper sweeps. Runners' idle-state changes happen
/// at human latency; sweeping once a minute is plenty.
pub const REAP_INTERVAL: Duration = Duration::from_secs(60);

/// What kind of runner this is. Today only `Text`; voice arrives
/// when the voice-pipeline crate is wired through (Phase 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunnerModality {
    Text,
    Voice,
}

impl RunnerModality {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Voice => "Voice",
        }
    }
}

/// One entry — what the Settings page shows per row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerEntry {
    pub conversation_id: String,
    /// Operator-friendly label: principal name, group title, etc.
    /// Free-form text; the runner doesn't enforce a shape.
    pub principal_label: Option<String>,
    pub modality: RunnerModality,
    /// `true` for the Controller's pinned thread (never idle-reaped).
    pub controller_runner: bool,
    /// Wall-clock when the runner first registered.
    pub started_at: DateTime<Utc>,
    /// Wall-clock when the most recent turn ended. The reaper compares
    /// `now - last_active_at` to `IDLE_TTL`.
    pub last_active_at: DateTime<Utc>,
    /// In-flight turn? Reaper never evicts a busy runner.
    pub in_flight: bool,
    /// Number of turns observed so far.
    pub turn_count: u64,
    /// Set on `restart()` until the next turn re-registers it.
    pub restart_pending: bool,
}

impl RunnerEntry {
    /// True when the entry's last activity is older than `IDLE_TTL`
    /// AND the runner isn't the controller AND no turn is in flight.
    fn should_reap(&self, now: DateTime<Utc>) -> bool {
        if self.controller_runner || self.in_flight {
            return false;
        }
        let age = now.signed_duration_since(self.last_active_at);
        match age.to_std() {
            Ok(d) => d >= IDLE_TTL,
            Err(_) => false,
        }
    }
}

/// Cloneable handle. Construct one in `AppState`; every route that
/// needs to snapshot the registry borrows from the same map.
#[derive(Clone, Debug, Default)]
pub struct RunnerRegistry {
    entries: Arc<DashMap<String, RunnerEntry>>,
}

impl RunnerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a turn as starting. Idempotent — if the conversation
    /// already has an entry, bump activity + flip `in_flight = true`.
    /// `controller_runner` semantics: any conversation whose
    /// `principal_label` resolves to a Controller principal — the
    /// caller is responsible for setting this correctly.
    pub fn register_turn_start(
        &self,
        conversation_id: &str,
        principal_label: Option<String>,
        modality: RunnerModality,
        controller_runner: bool,
    ) {
        let now = Utc::now();
        self.entries
            .entry(conversation_id.to_owned())
            .and_modify(|e| {
                e.last_active_at = now;
                e.in_flight = true;
                e.restart_pending = false;
                e.turn_count = e.turn_count.saturating_add(1);
                // Once a runner is observed as the controller, keep that flag
                // even if a later call passed false — guards against a stale
                // call site. The opposite (false → true) is a real promotion
                // and we accept it.
                if controller_runner {
                    e.controller_runner = true;
                }
            })
            .or_insert_with(|| RunnerEntry {
                conversation_id: conversation_id.to_owned(),
                principal_label,
                modality,
                controller_runner,
                started_at: now,
                last_active_at: now,
                in_flight: true,
                turn_count: 1,
                restart_pending: false,
            });
    }

    /// Mark a turn as ending (success or failure — either way the
    /// runner is no longer mid-turn). Updates `last_active_at` so
    /// the reaper measures from "just finished" not "just started."
    pub fn register_turn_end(&self, conversation_id: &str) {
        if let Some(mut entry) = self.entries.get_mut(conversation_id) {
            entry.last_active_at = Utc::now();
            entry.in_flight = false;
        }
    }

    /// Snapshot every runner for the Settings page.
    pub fn snapshot(&self) -> Vec<RunnerEntry> {
        let mut out: Vec<_> = self.entries.iter().map(|kv| kv.value().clone()).collect();
        // Stable order: in-flight first, then by last-active descending,
        // tiebreak by conversation_id so the SPA renders deterministically.
        out.sort_by(|a, b| {
            b.in_flight
                .cmp(&a.in_flight)
                .then_with(|| b.last_active_at.cmp(&a.last_active_at))
                .then_with(|| a.conversation_id.cmp(&b.conversation_id))
        });
        out
    }

    /// Operator-driven force-reap. Drops the entry from the registry
    /// and sets `restart_pending = true` for telemetry — when the
    /// next turn for this conversation runs, it'll register fresh
    /// (with `restart_pending = false`).
    ///
    /// Today this is a soft signal: in-process runners don't have a
    /// process to kill; they rehydrate from the event log every turn.
    /// Once container-managed runners are live, this is where we'll
    /// `SIGTERM` the container so any wedged in-flight work drains.
    pub fn restart(&self, conversation_id: &str) -> bool {
        match self.entries.get_mut(conversation_id) {
            Some(mut entry) => {
                entry.in_flight = false;
                entry.restart_pending = true;
                entry.last_active_at = Utc::now();
                debug!(
                    conversation_id,
                    "runner restart requested — entry retained for telemetry"
                );
                true
            }
            None => false,
        }
    }

    /// Run the reap pass. Returns the number of entries dropped.
    pub fn reap_idle(&self) -> usize {
        let now = Utc::now();
        let to_drop: Vec<String> = self
            .entries
            .iter()
            .filter(|kv| kv.value().should_reap(now))
            .map(|kv| kv.key().clone())
            .collect();
        for id in &to_drop {
            self.entries.remove(id);
        }
        if !to_drop.is_empty() {
            info!(reaped = to_drop.len(), "runner registry: reaped idle entries");
        }
        to_drop.len()
    }

    /// Long-running reaper task. Lives alongside the existing
    /// sweepers (log retention, ephemeral, refresh-token). Exits
    /// when `stop` is notified.
    pub async fn run_reaper(self, stop: Arc<Notify>) {
        info!(
            interval_secs = REAP_INTERVAL.as_secs(),
            ttl_secs = IDLE_TTL.as_secs(),
            "runner registry reaper running"
        );
        loop {
            tokio::select! {
                _ = tokio::time::sleep(REAP_INTERVAL) => {
                    self.reap_idle();
                }
                _ = stop.notified() => {
                    info!("runner registry reaper stopping");
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_minus(secs: i64) -> DateTime<Utc> {
        Utc::now() - chrono::Duration::seconds(secs)
    }

    #[test]
    fn register_turn_start_then_end_updates_in_flight() {
        let r = RunnerRegistry::new();
        r.register_turn_start("c1", Some("Alice".into()), RunnerModality::Text, false);
        let entry = r.snapshot().into_iter().next().unwrap();
        assert!(entry.in_flight);
        assert_eq!(entry.turn_count, 1);

        r.register_turn_end("c1");
        let entry = r.snapshot().into_iter().next().unwrap();
        assert!(!entry.in_flight);
    }

    #[test]
    fn re_register_increments_turn_count() {
        let r = RunnerRegistry::new();
        r.register_turn_start("c1", None, RunnerModality::Text, false);
        r.register_turn_end("c1");
        r.register_turn_start("c1", None, RunnerModality::Text, false);
        let entry = r.snapshot().into_iter().next().unwrap();
        assert_eq!(entry.turn_count, 2);
    }

    #[test]
    fn restart_drops_in_flight_and_marks_pending_but_keeps_entry() {
        let r = RunnerRegistry::new();
        r.register_turn_start("c1", None, RunnerModality::Text, false);
        assert!(r.restart("c1"));
        let entry = r.snapshot().into_iter().next().unwrap();
        assert!(!entry.in_flight);
        assert!(entry.restart_pending);
    }

    #[test]
    fn restart_returns_false_for_unknown_conversation() {
        let r = RunnerRegistry::new();
        assert!(!r.restart("nope"));
    }

    #[test]
    fn reap_idle_skips_controller_runner() {
        let r = RunnerRegistry::new();
        r.register_turn_start("c1", None, RunnerModality::Text, true);
        // Force the controller's last_active_at into the past beyond TTL.
        if let Some(mut entry) = r.entries.get_mut("c1") {
            entry.in_flight = false;
            entry.last_active_at = now_minus(IDLE_TTL.as_secs() as i64 * 2);
        }
        assert_eq!(r.reap_idle(), 0);
        assert_eq!(r.snapshot().len(), 1, "controller runner must survive");
    }

    #[test]
    fn reap_idle_drops_stale_non_controller() {
        let r = RunnerRegistry::new();
        r.register_turn_start("c1", None, RunnerModality::Text, false);
        if let Some(mut entry) = r.entries.get_mut("c1") {
            entry.in_flight = false;
            entry.last_active_at = now_minus(IDLE_TTL.as_secs() as i64 * 2);
        }
        assert_eq!(r.reap_idle(), 1);
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn reap_idle_skips_in_flight_even_when_old() {
        let r = RunnerRegistry::new();
        r.register_turn_start("c1", None, RunnerModality::Text, false);
        if let Some(mut entry) = r.entries.get_mut("c1") {
            // entry.in_flight stays true; even an ancient last_active_at
            // shouldn't drop a busy runner.
            entry.last_active_at = now_minus(IDLE_TTL.as_secs() as i64 * 2);
        }
        assert_eq!(r.reap_idle(), 0);
    }

    #[test]
    fn snapshot_orders_in_flight_first_then_recency() {
        let r = RunnerRegistry::new();
        r.register_turn_start("c-old", None, RunnerModality::Text, false);
        r.register_turn_end("c-old");
        if let Some(mut e) = r.entries.get_mut("c-old") {
            e.last_active_at = now_minus(120);
        }
        r.register_turn_start("c-recent", None, RunnerModality::Text, false);
        r.register_turn_end("c-recent");
        r.register_turn_start("c-busy", None, RunnerModality::Text, false);
        let names: Vec<_> = r
            .snapshot()
            .into_iter()
            .map(|e| e.conversation_id)
            .collect();
        assert_eq!(names[0], "c-busy", "in-flight first");
        assert_eq!(names[1], "c-recent", "more recent next");
        assert_eq!(names[2], "c-old", "oldest last");
    }
}
