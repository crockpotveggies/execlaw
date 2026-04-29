//! Per-principal-group runner supervisor.
//!
//! The supervisor owns:
//!   * the in-memory **registry** mapping `group_id → RunnerHandle`
//!     (the live WS connection, in-flight turn count, last-active
//!     timestamp, controller-pin flag),
//!   * **spawn / stop** lifecycle via `bollard`,
//!   * **registration auth** (one-time spawn secrets minted per
//!     `ensure(group_id)`, consumed when the runner phones home),
//!   * **forwarding** turn requests from `chats.rs` over each
//!     runner's WS and surfacing per-frame events back to the
//!     caller as a stream,
//!   * **reaping** idle runners (10 min for non-controller groups;
//!     never for controller groups) plus a per-turn max-duration
//!     watchdog that recovers wedged runners,
//!   * **workspace volume** lifecycle — created on first spawn,
//!     wiped only on idle reap / explicit "wipe workspace" /
//!     group delete.
//!
//! The supervisor does NOT own:
//!   * the event log (runners propose `EventLogAppend` frames; the
//!     supervisor signs + commits via `EventLog::append`),
//!   * the inference-backend lifecycle (`backend_supervisor` already
//!     owns that),
//!   * the SPA's WebSocket bus (`events::EventBus`) — the supervisor
//!     translates incoming `RunnerToServer` frames into `UiEvent`s
//!     and republishes on the existing bus.
//!
//! This module is the v1 skeleton: registry + auth + forwarding +
//! reaper. The bollard-driven spawn + workspace volume management
//! live in `runner_spawn.rs` (sibling). Tests in this file exercise
//! the registry + reaper logic with no Docker and no real WS — they
//! use the `RunnerHandle::test_handle()` constructor and a tokio
//! mpsc pair that stands in for the socket.

use crate::events::{EventBus, UiEvent};
use crate::runner_spawn::{RunnerLauncher, RunnerSpec};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use execlaw_core::Database;
use execlaw_core::principal_groups::PrincipalGroupStore;
use execlaw_runner_protocol::{
    RunnerToServer, ServerToRunner, ShutdownReason, ToolCallResult, TurnRequest,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, Notify, RwLock, mpsc};
use tracing::{info, warn};

/// Idle TTL for non-controller runners.
pub const IDLE_TTL: Duration = Duration::from_secs(10 * 60);

/// How often the reaper loop sweeps. Idle changes happen at human
/// latency; once a minute is plenty.
pub const REAP_INTERVAL: Duration = Duration::from_secs(60);

/// Hard upper bound on a single turn's wall-clock time. After this
/// the supervisor sends `CancelTurn`; if the runner doesn't honor
/// it within `STUCK_GRACE`, the supervisor force-kills the
/// container. Default 60 minutes — long enough for deep-research
/// sub-agents but short enough to recover a stuck runner before
/// the operator notices.
pub const MAX_TURN_DURATION: Duration = Duration::from_secs(60 * 60);

/// Grace period after a `CancelTurn` to give the runner a chance
/// to send `Error { cancelled: true }`. After this we treat the
/// runner as wedged.
pub const STUCK_GRACE: Duration = Duration::from_secs(5);

/// Frames the supervisor sends to a runner over its WS. Wrapped in
/// a tokio mpsc so the WS handler task can serialise + write
/// without lock contention from the public API surface.
pub type ServerToRunnerTx = mpsc::UnboundedSender<ServerToRunner>;

/// One in-flight turn's per-frame event stream. Frames flow from
/// the runner → supervisor → here. The chat handler consumes this
/// and translates each frame into the appropriate side effect
/// (broadcast on the WS bus, dispatch a tool, commit an event).
pub type TurnEventRx = mpsc::UnboundedReceiver<TurnEvent>;
pub type TurnEventTx = mpsc::UnboundedSender<TurnEvent>;

/// What the chat handler sees per turn. Mostly a flattened mirror
/// of `RunnerToServer` but pre-filtered to the events that turn
/// owner cares about (frames addressed to other turns are routed
/// elsewhere by the supervisor's read loop).
#[derive(Debug, Clone)]
pub enum TurnEvent {
    TokenDelta {
        text: String,
    },
    Phase {
        phase: String,
    },
    /// Runner asked to call a tool. Caller dispatches via the
    /// existing `ChainedToolDispatch` and replies on the
    /// supervisor's `submit_tool_result` API.
    ToolCallRequest {
        call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    /// Runner proposed an event log append. The chat handler that
    /// owns this turn is responsible for HMAC-signing + committing
    /// (SQLite is single-writer; the supervisor doesn't hold the
    /// event-log handle). Carries the full payload so the chat
    /// handler can encode + commit without round-tripping back to
    /// the runner.
    EventLogAppend {
        kind: String,
        payload: serde_json::Value,
        actor: Option<String>,
    },
    Complete {
        assistant_text: String,
        finish_reason: Option<String>,
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
    },
    Error {
        message: String,
        cancelled: bool,
    },
}

/// Persistent registry entry per principal group. Cheap to clone;
/// the heavy state lives behind `Arc<...>` so the registry can be
/// snapshotted without lock contention.
#[derive(Clone)]
pub struct RunnerHandle {
    pub group_id: String,
    pub controller_runner: bool,
    /// `None` until the runner WS-registers. Once set, frames sent
    /// to this channel are serialised and forwarded over the WS.
    pub tx: Arc<Mutex<Option<ServerToRunnerTx>>>,
    /// Per-turn event-stream forwarders. Keyed by `turn_id`. The
    /// supervisor's WS read loop dispatches incoming
    /// `RunnerToServer` frames into the matching `TurnEventTx`.
    pub turn_streams: Arc<DashMap<String, TurnEventTx>>,
    pub state: Arc<RwLock<RunnerState>>,
}

#[derive(Debug, Clone)]
pub struct RunnerState {
    pub status: RunnerStatus,
    pub started_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
    /// Set of in-flight `turn_id`s. Reaper considers the runner
    /// idle iff this is empty.
    pub in_flight_turns: HashSet<String>,
    /// Container id from bollard once spawned. None for in-process
    /// test handles.
    pub container_id: Option<String>,
    /// Per-turn deadline tracking. Maps `turn_id → deadline`. The
    /// watchdog scans this on every reap pass.
    pub turn_deadlines: std::collections::HashMap<String, DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerStatus {
    /// Container created (or about to be), runner has not yet
    /// completed WS registration.
    Spawning,
    /// WS registered, ready for turns.
    Ready,
    /// Supervisor sent `Shutdown`; waiting for the WS to close
    /// before the registry entry is dropped.
    Stopping,
    /// Container has exited (cleanly or via reap). Entry retained
    /// for telemetry until the next `ensure(group_id)`.
    Dead,
}

impl RunnerHandle {
    /// True when the runner is idle (no in-flight turns) AND its
    /// last activity is older than `IDLE_TTL`. Controller runners
    /// are never reapable. Read-only — the reaper still has to
    /// race-check inside the lock when it actually decides to act.
    pub async fn is_reapable(&self, now: DateTime<Utc>, ttl: Duration) -> bool {
        if self.controller_runner {
            return false;
        }
        let s = self.state.read().await;
        if !s.in_flight_turns.is_empty() {
            return false;
        }
        match (now - s.last_active_at).to_std() {
            Ok(d) => d >= ttl,
            Err(_) => false,
        }
    }

    /// Convenience: build an in-process handle with no real
    /// container/WS, used by tests that exercise registry / reaper
    /// logic without Docker.
    #[cfg(test)]
    pub fn test_handle(group_id: &str, controller: bool) -> Self {
        let now = Utc::now();
        Self {
            group_id: group_id.to_owned(),
            controller_runner: controller,
            tx: Arc::new(Mutex::new(None)),
            turn_streams: Arc::new(DashMap::new()),
            state: Arc::new(RwLock::new(RunnerState {
                status: RunnerStatus::Spawning,
                started_at: now,
                last_active_at: now,
                in_flight_turns: HashSet::new(),
                container_id: None,
                turn_deadlines: std::collections::HashMap::new(),
            })),
        }
    }
}

/// One pending spawn waiting for the runner to complete the WS
/// registration handshake. The supervisor stores the expected
/// secret in this map; the WS handler looks it up on incoming
/// auth, validates with constant-time-compare, and consumes the
/// entry.
#[derive(Clone)]
pub struct PendingSpawn {
    pub secret: [u8; 32],
    /// Notifies the spawner that the runner registered. Lets
    /// `ensure()` await the readiness handshake before returning.
    pub registered: Arc<Notify>,
}

/// Public, cloneable handle to the supervisor. Construct one in
/// `AppState`; route handlers + chat handlers borrow from the same
/// instance.
#[derive(Clone)]
pub struct RunnerSupervisor {
    inner: Arc<SupervisorInner>,
    /// Launcher used for lazy spawn from the chat path. `None`
    /// during tests + when the supervisor was constructed without
    /// `with_launcher` (in which case `ensure_for_group` returns
    /// an error and the chat path will surface "runner not
    /// available"). Keeping this Option here rather than a
    /// separate `Option<Arc<dyn RunnerLauncher>>` field on
    /// `AppState` means the chat handler doesn't have to plumb
    /// the launcher through every call site.
    launcher: Option<Arc<dyn crate::runner_spawn::RunnerLauncher>>,
    /// Template the supervisor uses when it has to spawn a fresh
    /// runner for an existing group (lazy spawn from the chat
    /// path, or operator restart). `group_id` + `spawn_secret_hex`
    /// are filled in per-spawn; everything else is reused.
    spec_template: Option<RunnerSpec>,
}

struct SupervisorInner {
    /// Live runners by `group_id`.
    runners: DashMap<String, RunnerHandle>,
    /// Spawn-in-progress map. `pending_spawns[group_id]` holds the
    /// expected secret + notify; populated by `ensure()`, consumed
    /// by the WS register handler.
    pending_spawns: DashMap<String, PendingSpawn>,
    /// Monotonic counter for `turn_id`s minted server-side.
    next_turn_seq: AtomicU64,
    /// Event bus for translating runner frames into SPA WS events.
    pub events: EventBus,
    /// Database handle for principal-group `last_active_at` writes
    /// + event log proxy commits.
    pub db: Database,
}

impl RunnerSupervisor {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self {
            inner: Arc::new(SupervisorInner {
                runners: DashMap::new(),
                pending_spawns: DashMap::new(),
                next_turn_seq: AtomicU64::new(1),
                events,
                db,
            }),
            launcher: None,
            spec_template: None,
        }
    }

    /// Attach a launcher + base spec so the chat hot path can
    /// lazy-spawn runners when a turn lands for a group that
    /// doesn't have one yet. Builder-style so test harnesses can
    /// keep using `RunnerSupervisor::new(...)` without rewriting.
    pub fn with_launcher(
        mut self,
        launcher: Arc<dyn crate::runner_spawn::RunnerLauncher>,
        spec_template: RunnerSpec,
    ) -> Self {
        self.launcher = Some(launcher);
        self.spec_template = Some(spec_template);
        self
    }

    /// Lazy-spawn entry point for the chat path. Returns the
    /// existing handle when one is registered, otherwise spawns a
    /// fresh container (using the launcher + spec_template
    /// configured via `with_launcher`) and awaits its WS
    /// registration. Errors when:
    ///   * no launcher is configured (test-mode supervisor), OR
    ///   * the underlying ensure_runner fails (Docker error /
    ///     timeout).
    pub async fn ensure_for_group(
        &self,
        group_id: &str,
        timeout: Duration,
    ) -> Result<RunnerHandle, EnsureError> {
        if let Some(h) = self.get(group_id) {
            // Existing handle — only reuse if it's actually usable.
            // 2026-04-28: a `Stopping` or `Dead` entry is a stale
            // tombstone (operator clicked Restart/Wipe, or the WS
            // dropped without our close handler firing). Returning
            // it would point the chat handler at a runner whose tx
            // channel goes nowhere. Drop it from the registry and
            // fall through to a fresh spawn.
            let status = h.state.read().await.status;
            match status {
                RunnerStatus::Ready | RunnerStatus::Spawning => {
                    let mut s = h.state.write().await;
                    s.last_active_at = Utc::now();
                    return Ok(h.clone());
                }
                RunnerStatus::Stopping | RunnerStatus::Dead => {
                    info!(
                        group_id,
                        ?status,
                        "ensure_for_group: dropping stale handle, will respawn"
                    );
                    self.inner.runners.remove(group_id);
                }
            }
        }
        let launcher = self
            .launcher
            .as_ref()
            .ok_or_else(|| EnsureError::Spawn("no launcher configured".into()))?;
        let template = self
            .spec_template
            .clone()
            .ok_or_else(|| EnsureError::Spawn("no spec template configured".into()))?;
        self.ensure_runner(launcher.as_ref(), group_id, template, timeout)
            .await
    }

    /// Mint a unique `turn_id`. The seq is per-process — fine
    /// because the runner just echoes it; no cross-process
    /// uniqueness needed.
    /// Borrow the event bus the supervisor publishes on. Tests +
    /// the chat handler subscribe through this so the
    /// supervisor's internal `Arc<SupervisorInner>` stays
    /// encapsulated.
    pub fn events(&self) -> &EventBus {
        &self.inner.events
    }

    /// Borrow the database. Used by `chats.rs` when it commits a
    /// runner-proposed `EventLogAppend` frame on behalf of the
    /// runner — SQLite stays single-writer.
    pub fn db(&self) -> &Database {
        &self.inner.db
    }

    /// Spawn (or return existing) a runner for `group_id`. Awaits
    /// the WS registration handshake before returning so callers
    /// can immediately `forward_turn` against the result.
    ///
    /// The spawn flow:
    ///   1. If a runner is already registered → return its handle.
    ///   2. Otherwise mint a fresh spawn secret, call
    ///      `launcher.spawn(spec)` to start the container, await
    ///      the `register_pending_spawn` notify (set by
    ///      `accept_registration`) up to `timeout`.
    ///   3. Update the registry entry with the container id +
    ///      controller-pin flag from `state_principal_groups`.
    ///   4. Return the handle.
    pub async fn ensure_runner<L: RunnerLauncher + ?Sized>(
        &self,
        launcher: &L,
        group_id: &str,
        spec: RunnerSpec,
        timeout: Duration,
    ) -> Result<RunnerHandle, EnsureError> {
        if let Some(h) = self.get(group_id) {
            // Same stale-handle guard as `ensure_for_group`:
            // Stopping/Dead entries are tombstones, not live
            // runners. Drop and respawn.
            let status = h.state.read().await.status;
            match status {
                RunnerStatus::Ready | RunnerStatus::Spawning => {
                    let mut s = h.state.write().await;
                    s.last_active_at = Utc::now();
                    return Ok(h.clone());
                }
                RunnerStatus::Stopping | RunnerStatus::Dead => {
                    info!(
                        group_id,
                        ?status,
                        "ensure_runner: dropping stale handle, will respawn"
                    );
                    self.inner.runners.remove(group_id);
                }
            }
        }

        let (secret, registered) = self.register_pending_spawn(group_id);

        // Bake the secret into the spec we hand to the launcher.
        let mut spec = spec;
        spec.spawn_secret_hex = hex::encode(secret);
        spec.group_id = group_id.to_owned();

        let id = launcher
            .spawn(&spec)
            .await
            .map_err(|e| EnsureError::Spawn(format!("{e}")))?;

        // Wait for the runner to phone home + complete the WS
        // registration. `register_pending_spawn`'s Notify is
        // notified by `accept_registration` on success
        // (`notify_one`, so a permit waits even if the registration
        // beat us to the await).
        //
        // Belt-and-suspenders: poll the registry every 100ms during
        // the wait so a *missed* permit (impossible with notify_one
        // but cheap to guard against) still recovers. Also lets us
        // exit early once the registration lands without waiting
        // for tokio's timer wheel.
        let deadline = tokio::time::Instant::now() + timeout;
        let mut registered_fut = std::pin::pin!(registered.notified());
        loop {
            tokio::select! {
                _ = registered_fut.as_mut() => break,
                _ = tokio::time::sleep_until(deadline) => {
                    // Timed out. Best-effort cleanup of the half-
                    // started container so we don't leak.
                    let _ = launcher.kill(&id.container_id).await;
                    self.inner.pending_spawns.remove(group_id);
                    return Err(EnsureError::Timeout);
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    // Poll the registry — if accept_registration
                    // already inserted, the runner is up even
                    // though our notify never woke (shouldn't
                    // happen with notify_one, but cheap guard).
                    if self.inner.runners.contains_key(group_id) {
                        break;
                    }
                }
            }
        }

        // Stamp the container id + controller-pin flag.
        let store = PrincipalGroupStore::new(&self.inner.db);
        let controller_runner = match store.get(group_id) {
            Ok(Some(g)) => g.includes_controller,
            _ => false,
        };
        if let Some(handle) = self.get(group_id) {
            let mut s = handle.state.write().await;
            s.container_id = Some(id.container_id.clone());
            // Re-affirm controller-pin (acceptance handler may
            // have used a default).
            drop(s);
            // controller_runner is a top-level field, not in
            // RunnerState. Re-insert with the corrected value.
            let mut updated = handle.clone();
            updated.controller_runner = controller_runner;
            self.inner.runners.insert(group_id.to_owned(), updated);
            return Ok(self.get(group_id).expect("just inserted"));
        }
        Err(EnsureError::Spawn(
            "runner registered but registry entry vanished".into(),
        ))
    }

    /// Full reap path: send Shutdown frame, wait for the WS to
    /// close (or the grace window), kill the container, drop
    /// the registry entry, and (when the reason warrants it) wipe
    /// the workspace volume.
    ///
    /// Volume policy:
    ///   * `IdleReap` / `OperatorWipe` / `GroupDeleted` → wipe
    ///   * `OperatorRestart` / `ServerShutdown` → preserve
    ///
    /// Idempotent — calling `reap_runner` twice on the same group
    /// is a no-op the second time.
    pub async fn reap_runner<L: RunnerLauncher + ?Sized>(
        &self,
        launcher: &L,
        group_id: &str,
        reason: ShutdownReason,
    ) -> Result<ReapReport, ReapError> {
        let handle = match self.inner.runners.get(group_id) {
            Some(h) => h.value().clone(),
            None => return Ok(ReapReport::default()),
        };
        if handle.controller_runner && reason == ShutdownReason::IdleReap {
            return Err(ReapError::ControllerProtected);
        }

        // Snapshot container id BEFORE we tell the runner to shut
        // down, so even if the registry entry gets cleared by
        // `drop_registration` (WS close handler) we still know
        // which container to kill.
        let container_id = {
            let s = handle.state.read().await;
            s.container_id.clone()
        };
        {
            let mut s = handle.state.write().await;
            s.status = RunnerStatus::Stopping;
        }

        // Polite shutdown frame. Runner exits its main loop and
        // closes the WS; the `runner_rpc` read loop sees the close
        // and calls `drop_registration` for us.
        let _ = send_to_runner(&handle, ServerToRunner::Shutdown { reason }).await;

        // Wait briefly for the WS to actually close (registry
        // entry to disappear). Cap at 5s — runner has had its
        // chance.
        let close_deadline =
            tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < close_deadline {
            if self.get(group_id).is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Force-evict registry entry if the runner didn't close
        // gracefully.
        self.drop_registration(group_id).await;

        // Kill the container (idempotent — `kill` on a
        // non-existent container is a no-op for the bollard
        // launcher).
        if let Some(cid) = container_id {
            if let Err(e) = launcher.kill(&cid).await {
                warn!(
                    group_id,
                    container_id = %cid,
                    error = %e,
                    "reap_runner: kill failed (continuing)"
                );
            }
        }

        // Wipe the volume when the reason calls for it.
        let mut wiped_volume = false;
        let wipe = matches!(
            reason,
            ShutdownReason::IdleReap
                | ShutdownReason::OperatorWipe
                | ShutdownReason::GroupDeleted
        );
        if wipe {
            // Sanity: never wipe the controller's workspace via
            // an idle reap (defence-in-depth — earlier check
            // should've caught it).
            if handle.controller_runner && reason == ShutdownReason::IdleReap {
                warn!(
                    group_id,
                    "controller workspace wipe blocked at last gate"
                );
            } else {
                match launcher.wipe_volume(group_id).await {
                    Ok(_) => {
                        wiped_volume = true;
                        info!(group_id, ?reason, "wiped workspace volume");
                    }
                    Err(e) => {
                        warn!(group_id, error = %e, "wipe_volume failed");
                    }
                }
            }
        }
        Ok(ReapReport {
            killed_container: handle.state.read().await.container_id.clone(),
            wiped_volume,
        })
    }

    /// Prewarm the controller's runner on server boot. Resolves
    /// `(web, {controller})` (creating the principal group row if
    /// needed), then `ensure_runner` to spawn + register. The
    /// controller's runner stays hot for the rest of the process
    /// lifetime by virtue of `controller_runner = true` blocking
    /// the idle reaper.
    ///
    /// Best-effort: returns Err if the prewarm path fails, but the
    /// caller (CLI's `cmd_serve`) only logs the error and keeps
    /// going so a busted Docker daemon doesn't break the whole
    /// server.
    pub async fn prewarm_controller<L: RunnerLauncher + ?Sized>(
        &self,
        launcher: &L,
        controller_principal_id: &str,
        spec_template: RunnerSpec,
        timeout: Duration,
    ) -> Result<RunnerHandle, EnsureError> {
        use execlaw_core::ids::PrincipalId;
        use execlaw_core::principal_groups::{GroupKey, PrincipalGroupStore};
        let store = PrincipalGroupStore::new(&self.inner.db);
        let principals = vec![PrincipalId::from(controller_principal_id)];
        let now = Utc::now().timestamp();
        let group = store
            .resolve(
                &GroupKey {
                    channel: "web",
                    native_group_id: None,
                    principals: &principals,
                    includes_controller: true,
                },
                now,
            )
            .map_err(|e| EnsureError::Spawn(format!("resolve group: {e}")))?;
        info!(
            group_id = %group.group_id,
            "prewarming controller runner"
        );
        self.ensure_runner(launcher, &group.group_id, spec_template, timeout)
            .await
    }

    /// Idle-reap pass that kills containers + wipes volumes for
    /// every reapable runner. Replaces the WS-only `reap_idle()`
    /// path used by tests; the production reaper task should use
    /// this method instead.
    pub async fn reap_idle_with_launcher<L: RunnerLauncher + ?Sized>(
        &self,
        launcher: &L,
    ) -> Vec<String> {
        let now = Utc::now();
        let mut reaped = Vec::new();
        for handle in self.snapshot() {
            if handle.is_reapable(now, IDLE_TTL).await {
                if let Err(e) = self
                    .reap_runner(launcher, &handle.group_id, ShutdownReason::IdleReap)
                    .await
                {
                    warn!(group_id = %handle.group_id, error = %e, "reap_runner failed");
                    continue;
                }
                reaped.push(handle.group_id.clone());
            }
        }
        if !reaped.is_empty() {
            info!(reaped_count = reaped.len(), "runner supervisor: idle-reaped runners");
        }
        reaped
    }

    pub fn mint_turn_id(&self) -> String {
        let n = self.inner.next_turn_seq.fetch_add(1, Ordering::Relaxed);
        format!("turn-{n}-{}", uuid::Uuid::new_v4())
    }

    /// Register a fresh pending spawn. Returns the secret the
    /// caller must pass to the runner via env. If a spawn is
    /// already pending for this group, the prior secret is
    /// invalidated (replaced) — the most recent caller wins.
    pub fn register_pending_spawn(&self, group_id: &str) -> ([u8; 32], Arc<Notify>) {
        let mut secret = [0u8; 32];
        rand::Rng::fill(&mut rand::thread_rng(), &mut secret);
        let pending = PendingSpawn {
            secret,
            registered: Arc::new(Notify::new()),
        };
        let registered = pending.registered.clone();
        self.inner
            .pending_spawns
            .insert(group_id.to_owned(), pending);
        (secret, registered)
    }

    /// Validate a registration attempt against `pending_spawns`.
    /// Returns Ok with a fresh `RunnerHandle` on match (entry
    /// consumed); Err on mismatch or missing.
    pub fn accept_registration(
        &self,
        group_id: &str,
        bearer_secret: &[u8],
        controller_runner: bool,
    ) -> Result<RunnerHandle, RegistrationError> {
        let pending = self
            .inner
            .pending_spawns
            .remove(group_id)
            .map(|(_, v)| v)
            .ok_or(RegistrationError::NoPendingSpawn)?;
        if !constant_time_eq(&pending.secret, bearer_secret) {
            return Err(RegistrationError::SecretMismatch);
        }

        let handle = RunnerHandle {
            group_id: group_id.to_owned(),
            controller_runner,
            tx: Arc::new(Mutex::new(None)),
            turn_streams: Arc::new(DashMap::new()),
            state: Arc::new(RwLock::new(RunnerState {
                status: RunnerStatus::Ready,
                started_at: Utc::now(),
                last_active_at: Utc::now(),
                in_flight_turns: HashSet::new(),
                container_id: None,
                turn_deadlines: std::collections::HashMap::new(),
            })),
        };
        self.inner
            .runners
            .insert(group_id.to_owned(), handle.clone());
        // 2026-04-28: was `notify_waiters()` which only wakes
        // futures that are ALREADY polling. With a fast-spawning
        // Docker container the runner can WS-register before
        // `ensure_runner` reaches its `tokio::time::timeout(...)`
        // await — the notification fires with no waiters and is
        // lost, then `ensure_runner` waits 30s for a notification
        // that never comes again. `notify_one()` keeps a permit
        // for the next waiter so a late await still wakes
        // immediately.
        pending.registered.notify_one();
        Ok(handle)
    }

    /// Look up a registered runner. Returns `None` if no live
    /// runner exists for `group_id` (caller may need to call
    /// `ensure(group_id)` to spawn one — that path lands when the
    /// real bollard spawn is wired in).
    pub fn get(&self, group_id: &str) -> Option<RunnerHandle> {
        self.inner.runners.get(group_id).map(|kv| kv.value().clone())
    }

    /// Snapshot every live runner. Used by the reaper sweep + the
    /// admin API.
    pub fn snapshot(&self) -> Vec<RunnerHandle> {
        self.inner
            .runners
            .iter()
            .map(|kv| kv.value().clone())
            .collect()
    }

    /// Drop a runner's registry entry. The caller is responsible
    /// for closing the WS first (the runner-side will see EOF and
    /// exit).
    pub fn forget(&self, group_id: &str) {
        self.inner.runners.remove(group_id);
    }

    /// Forward a turn to a runner. Caller awaits the returned
    /// `TurnEventRx` and pumps frames out to the SPA / event log.
    /// Returns Err when no runner is registered for this group.
    pub async fn forward_turn(
        &self,
        group_id: &str,
        request: TurnRequest,
    ) -> Result<TurnEventRx, ForwardError> {
        let handle = self
            .inner
            .runners
            .get(group_id)
            .map(|kv| kv.value().clone())
            .ok_or(ForwardError::NoRunner)?;

        // Build the per-turn event channel + register it with the
        // runner's read-loop dispatcher.
        let (tx, rx) = mpsc::unbounded_channel::<TurnEvent>();
        handle
            .turn_streams
            .insert(request.turn_id.clone(), tx);
        {
            let mut s = handle.state.write().await;
            s.in_flight_turns.insert(request.turn_id.clone());
            s.turn_deadlines.insert(
                request.turn_id.clone(),
                Utc::now() + chrono::Duration::from_std(MAX_TURN_DURATION).unwrap(),
            );
        }

        // Push the Turn frame onto the runner's send queue.
        let frame = ServerToRunner::Turn(request.clone());
        send_to_runner(&handle, frame)
            .await
            .map_err(|_| ForwardError::RunnerGone)?;

        Ok(rx)
    }

    /// Operator-driven turn cancellation (the existing stop button
    /// fan-out). Sets a flag in the runner; the runner's streaming
    /// loop polls between chunks and emits
    /// `Error { cancelled: true }`.
    pub async fn cancel_turn(&self, group_id: &str, turn_id: &str) -> bool {
        let Some(handle) = self.get(group_id) else {
            return false;
        };
        let frame = ServerToRunner::CancelTurn {
            turn_id: turn_id.to_owned(),
        };
        send_to_runner(&handle, frame).await.is_ok()
    }

    /// Reply to a tool-call request. Dispatched by the chat
    /// handler after `ChainedToolDispatch::call()` returns.
    pub async fn submit_tool_result(
        &self,
        group_id: &str,
        result: ToolCallResult,
    ) -> bool {
        let Some(handle) = self.get(group_id) else {
            return false;
        };
        let frame = ServerToRunner::ToolCallResult(result);
        send_to_runner(&handle, frame).await.is_ok()
    }

    /// Sweep all runners, reaping any whose `last_active_at` is
    /// older than `IDLE_TTL` (and whose in-flight set is empty,
    /// and which aren't the controller). Returns the list of
    /// reaped `group_id`s for telemetry.
    pub async fn reap_idle(&self) -> Vec<String> {
        let now = Utc::now();
        let mut reaped = Vec::new();
        let snap = self.snapshot();
        for handle in snap {
            if handle.is_reapable(now, IDLE_TTL).await {
                if let Err(e) = self.reap_group(&handle.group_id, ShutdownReason::IdleReap).await {
                    warn!(group_id = %handle.group_id, error = %e, "reap_group failed");
                    continue;
                }
                reaped.push(handle.group_id.clone());
            }
        }
        if !reaped.is_empty() {
            info!(reaped_count = reaped.len(), "runner supervisor reaped idle runners");
        }
        reaped
    }

    /// Send the supervisor's reap path: graceful shutdown frame,
    /// wait briefly for runner ack, drop registry entry. The
    /// container kill + volume removal are layered on top by the
    /// caller (or the bollard-backed `runner_spawn` module);
    /// `reap_group` itself only handles the WS-level dance.
    pub async fn reap_group(
        &self,
        group_id: &str,
        reason: ShutdownReason,
    ) -> Result<(), ReapError> {
        let handle = self
            .inner
            .runners
            .get(group_id)
            .map(|kv| kv.value().clone())
            .ok_or(ReapError::UnknownGroup)?;
        // Belt-and-suspenders: never reap a controller runner via
        // an idle-reason path. Operator wipe / explicit delete
        // carry the explicit policy override via a non-IdleReap
        // reason.
        if handle.controller_runner && reason == ShutdownReason::IdleReap {
            return Err(ReapError::ControllerProtected);
        }

        // 2026-04-28 — when a launcher is configured (production
        // path), do the FULL reap dance: WS shutdown + drop_registration
        // + container kill + (when reason calls for it) volume wipe.
        // Previously this was a WS-only stub that left the entry
        // in `Stopping` forever if the runner-binary didn't ack
        // the Shutdown frame, with no way for the operator to
        // recover from the SPA. The tests that didn't wire a
        // launcher (no-Docker test fixtures) fall through to the
        // WS-only path below.
        if let Some(launcher) = self.launcher.clone() {
            let _ = self
                .reap_runner(launcher.as_ref(), group_id, reason)
                .await
                .map_err(|e| {
                    warn!(group_id, error = %e, "reap_group: full reap failed");
                });
            return Ok(());
        }

        // Test / no-launcher fallback.
        {
            let mut s = handle.state.write().await;
            s.status = RunnerStatus::Stopping;
        }
        let frame = ServerToRunner::Shutdown { reason };
        let _ = send_to_runner(&handle, frame).await;
        Ok(())
    }

    /// Long-running reaper task. Lives next to the existing
    /// runner-registry reaper / log-retention sweepers.
    pub async fn run_reaper(self, stop: Arc<Notify>) {
        info!(
            interval_secs = REAP_INTERVAL.as_secs(),
            ttl_secs = IDLE_TTL.as_secs(),
            max_turn_secs = MAX_TURN_DURATION.as_secs(),
            "runner supervisor reaper running"
        );
        loop {
            tokio::select! {
                _ = tokio::time::sleep(REAP_INTERVAL) => {
                    let _ = self.reap_idle().await;
                    self.watchdog_pass().await;
                    self.touch_active_groups().await;
                }
                _ = stop.notified() => {
                    info!("runner supervisor reaper stopping");
                    return;
                }
            }
        }
    }

    /// Per-turn watchdog: cancel any turn that's been running
    /// longer than `MAX_TURN_DURATION`. Sends `CancelTurn`; the
    /// follow-up "still didn't honor" → SIGTERM container path
    /// belongs to `runner_spawn`.
    pub async fn watchdog_pass(&self) {
        let now = Utc::now();
        for handle in self.snapshot() {
            // Snapshot the deadlines briefly to avoid holding the
            // write lock during a send.
            let to_cancel: Vec<String> = {
                let s = handle.state.read().await;
                s.turn_deadlines
                    .iter()
                    .filter(|(_, d)| now >= **d)
                    .map(|(k, _)| k.clone())
                    .collect()
            };
            for turn_id in to_cancel {
                warn!(
                    group_id = %handle.group_id,
                    turn_id = %turn_id,
                    "max-turn watchdog: cancelling stuck turn"
                );
                let frame = ServerToRunner::CancelTurn { turn_id: turn_id.clone() };
                let _ = send_to_runner(&handle, frame).await;
            }
        }
    }

    /// Push every live runner's `last_active_at` back into the
    /// `state_principal_groups` table so the boot-time orphan
    /// sweep + admin-page sort have fresh values.
    async fn touch_active_groups(&self) {
        let store = PrincipalGroupStore::new(&self.inner.db);
        let now = Utc::now().timestamp();
        for handle in self.snapshot() {
            let s = handle.state.read().await;
            if !s.in_flight_turns.is_empty() {
                let _ = store.touch_active(&handle.group_id, now);
            }
        }
    }

    /// Boot-time orphan sweep: list every `execlaw-runner-*`
    /// volume the daemon knows about; for each, check whether
    /// `state_principal_groups` still has the corresponding row.
    /// Volumes for groups that no longer exist (and aren't the
    /// controller's — but a deleted controller group is still a
    /// valid orphan) are wiped.
    ///
    /// Runs once on server boot, before the supervisor accepts any
    /// inbound traffic. Idempotent: safe to call multiple times.
    /// Returns the list of `group_id`s whose volumes were wiped.
    pub async fn boot_orphan_sweep<L: crate::runner_spawn::RunnerLauncher + ?Sized>(
        &self,
        launcher: &L,
    ) -> Vec<String> {
        use crate::runner_spawn::volume_name_for;

        let store = PrincipalGroupStore::new(&self.inner.db);
        let known_groups = match store.list_all() {
            Ok(g) => g,
            Err(e) => {
                warn!(error = %e, "boot_orphan_sweep: list_all failed");
                return Vec::new();
            }
        };
        let known_ids: std::collections::HashSet<String> =
            known_groups.iter().map(|g| g.group_id.clone()).collect();

        let volumes = match launcher.list_runner_volumes().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "boot_orphan_sweep: list_runner_volumes failed");
                return Vec::new();
            }
        };
        let prefix = "execlaw-runner-";
        let mut wiped = Vec::new();
        for vol in volumes {
            let Some(group_id) = vol.strip_prefix(prefix) else {
                continue;
            };
            // Sanity check: only sweep if name actually matches our
            // expected pattern.
            if vol != volume_name_for(group_id) {
                continue;
            }
            if known_ids.contains(group_id) {
                // Active group; volume is expected. Leave it.
                continue;
            }
            info!(
                group_id = %group_id,
                "boot_orphan_sweep: removing orphan workspace volume"
            );
            match launcher.wipe_volume(group_id).await {
                Ok(_) => wiped.push(group_id.to_owned()),
                Err(e) => {
                    warn!(
                        group_id = %group_id,
                        error = %e,
                        "boot_orphan_sweep: wipe failed"
                    );
                }
            }
        }
        if !wiped.is_empty() {
            info!(
                wiped_count = wiped.len(),
                "boot_orphan_sweep: removed orphan workspace volumes"
            );
        }
        wiped
    }

    /// Internal: called by the WS read loop when a frame arrives
    /// from a registered runner. Routes to the right per-turn
    /// stream and updates state.
    pub async fn handle_inbound(&self, group_id: &str, frame: RunnerToServer) {
        let Some(handle) = self.get(group_id) else {
            warn!(group_id, "inbound frame for unknown group");
            return;
        };
        // Translate + dispatch.
        match frame {
            RunnerToServer::TokenDelta {
                turn_id,
                conversation_id,
                text,
            } => {
                self.inner.events.publish(UiEvent::ChatTokenDelta {
                    conversation_id: conversation_id.clone(),
                    text: text.clone(),
                });
                if let Some(tx) = handle.turn_streams.get(&turn_id) {
                    let _ = tx.send(TurnEvent::TokenDelta { text });
                }
            }
            RunnerToServer::Phase {
                turn_id,
                conversation_id,
                phase,
            } => {
                self.inner.events.publish(UiEvent::ConversationPhaseChanged {
                    conversation_id: conversation_id.clone(),
                    phase: phase.clone(),
                });
                if let Some(tx) = handle.turn_streams.get(&turn_id) {
                    let _ = tx.send(TurnEvent::Phase { phase });
                }
            }
            RunnerToServer::ToolCallRequest {
                turn_id,
                conversation_id: _,
                call_id,
                tool_name,
                args,
            } => {
                if let Some(tx) = handle.turn_streams.get(&turn_id) {
                    let _ = tx.send(TurnEvent::ToolCallRequest {
                        call_id,
                        tool_name,
                        args,
                    });
                }
            }
            RunnerToServer::EventLogAppend {
                turn_id,
                conversation_id: _,
                kind,
                payload,
                actor,
            } => {
                // Forward verbatim to the chat handler — it owns
                // the EventLog handle + the turn_seq context, so
                // it does the HMAC sign + commit. SQLite stays
                // single-writer.
                if let Some(tx) = handle.turn_streams.get(&turn_id) {
                    let _ = tx.send(TurnEvent::EventLogAppend {
                        kind,
                        payload,
                        actor,
                    });
                }
            }
            RunnerToServer::TurnComplete {
                turn_id,
                conversation_id,
                assistant_text,
                finish_reason,
                prompt_tokens,
                completion_tokens,
            } => {
                self.inner.events.publish(UiEvent::ChatMessageOutbound {
                    conversation_id,
                    seq: 0,
                    text: assistant_text.clone(),
                });
                if let Some(tx) = handle.turn_streams.get(&turn_id) {
                    let _ = tx.send(TurnEvent::Complete {
                        assistant_text,
                        finish_reason,
                        prompt_tokens,
                        completion_tokens,
                    });
                }
                self.finish_turn(&handle, &turn_id).await;
            }
            RunnerToServer::Error {
                turn_id,
                conversation_id: _,
                message,
                cancelled,
            } => {
                if let Some(tx) = handle.turn_streams.get(&turn_id) {
                    let _ = tx.send(TurnEvent::Error {
                        message,
                        cancelled,
                    });
                }
                self.finish_turn(&handle, &turn_id).await;
            }
            RunnerToServer::HeartbeatAck { .. } => {
                // No-op for v1 — we'll wire RTT tracking later.
            }
        }
    }

    async fn finish_turn(&self, handle: &RunnerHandle, turn_id: &str) {
        handle.turn_streams.remove(turn_id);
        let mut s = handle.state.write().await;
        s.in_flight_turns.remove(turn_id);
        s.turn_deadlines.remove(turn_id);
        s.last_active_at = Utc::now();
    }

    /// Called by the WS handler when the socket closes (cleanly or
    /// because the runner crashed). Drops the registry entry and
    /// fails any in-flight turn streams so chat handlers stop
    /// awaiting forever.
    pub async fn drop_registration(&self, group_id: &str) {
        let Some((_, handle)) = self.inner.runners.remove(group_id) else {
            return;
        };
        // Tear down every in-flight turn with an Error frame so
        // the chat handler sees a clean end.
        let to_close: Vec<String> =
            handle.turn_streams.iter().map(|kv| kv.key().clone()).collect();
        for turn_id in to_close {
            if let Some((_, tx)) = handle.turn_streams.remove(&turn_id) {
                let _ = tx.send(TurnEvent::Error {
                    message: "runner disconnected".into(),
                    cancelled: false,
                });
            }
        }
        let mut s = handle.state.write().await;
        s.status = RunnerStatus::Dead;
        s.in_flight_turns.clear();
        s.turn_deadlines.clear();
    }

    /// Set the runner's outbound channel after the WS handler has
    /// spawned its writer task.
    pub async fn attach_tx(&self, group_id: &str, tx: ServerToRunnerTx) {
        if let Some(handle) = self.get(group_id) {
            *handle.tx.lock().await = Some(tx);
        }
    }
}

/// Send a frame to a registered runner via its outbound channel.
/// Returns Err when no channel is attached (runner registered but
/// the WS writer task hasn't claimed it yet — vanishingly small
/// window; treat as a transient error).
async fn send_to_runner(
    handle: &RunnerHandle,
    frame: ServerToRunner,
) -> Result<(), SendError> {
    let guard = handle.tx.lock().await;
    let tx = guard.as_ref().ok_or(SendError::NotAttached)?;
    tx.send(frame).map_err(|_| SendError::Closed)?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("runner outbound channel not attached yet")]
    NotAttached,
    #[error("runner outbound channel closed")]
    Closed,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistrationError {
    #[error("no pending spawn for this group_id")]
    NoPendingSpawn,
    #[error("registration secret mismatch")]
    SecretMismatch,
}

#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    #[error("no runner registered for this group_id")]
    NoRunner,
    #[error("runner gone (WS closed) before turn could be sent")]
    RunnerGone,
}

#[derive(Debug, thiserror::Error)]
pub enum ReapError {
    #[error("unknown group_id")]
    UnknownGroup,
    #[error("controller groups are protected from idle reap")]
    ControllerProtected,
}

#[derive(Debug, Default, Clone)]
pub struct ReapReport {
    pub killed_container: Option<String>,
    pub wiped_volume: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum EnsureError {
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("timed out waiting for runner registration")]
    Timeout,
}

/// Constant-time byte slice comparison — guards the registration
/// secret check against timing attacks.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::DbConfig;
    use execlaw_core::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn fresh_supervisor() -> RunnerSupervisor {
        RunnerSupervisor::new(fresh_db(), EventBus::new())
    }

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn mint_turn_id_is_unique_and_monotone() {
        let s = fresh_supervisor();
        let a = s.mint_turn_id();
        let b = s.mint_turn_id();
        assert_ne!(a, b);
        assert!(a.starts_with("turn-1-"));
        assert!(b.starts_with("turn-2-"));
    }

    #[test]
    fn register_pending_spawn_overwrites_previous() {
        let s = fresh_supervisor();
        let (sec1, _) = s.register_pending_spawn("g-1");
        let (sec2, _) = s.register_pending_spawn("g-1");
        // The first secret is replaced; only the second works.
        assert_ne!(sec1, sec2);
        let r1 = s.accept_registration("g-1", &sec1, false);
        assert!(matches!(r1, Err(RegistrationError::SecretMismatch)));
    }

    #[test]
    fn accept_registration_consumes_entry_on_mismatch() {
        // Belt-and-suspenders security posture: a wrong secret is
        // a one-shot failure. The pending spawn is consumed and
        // the supervisor demands a fresh `register_pending_spawn`
        // before trying again. This way an attacker who guesses
        // wrong doesn't get a second guess on the same secret.
        let s = fresh_supervisor();
        let (secret, _) = s.register_pending_spawn("g-1");
        // Wrong group — no pending spawn for that group, original
        // pending entry untouched.
        assert!(matches!(
            s.accept_registration("g-other", &secret, false),
            Err(RegistrationError::NoPendingSpawn)
        ));
        // Wrong secret — mismatch AND entry consumed.
        let mut wrong = secret;
        wrong[0] ^= 0xff;
        assert!(matches!(
            s.accept_registration("g-1", &wrong, false),
            Err(RegistrationError::SecretMismatch)
        ));
        // Even the correct secret fails now — entry is gone.
        assert!(matches!(
            s.accept_registration("g-1", &secret, false),
            Err(RegistrationError::NoPendingSpawn)
        ));
    }

    #[test]
    fn accept_registration_succeeds_with_correct_secret() {
        let s = fresh_supervisor();
        let (secret, _) = s.register_pending_spawn("g-1");
        let h = s.accept_registration("g-1", &secret, false).unwrap();
        assert_eq!(h.group_id, "g-1");
        assert!(s.get("g-1").is_some());
        // Second use of the same secret fails — entry consumed.
        assert!(matches!(
            s.accept_registration("g-1", &secret, false),
            Err(RegistrationError::NoPendingSpawn)
        ));
    }

    #[tokio::test]
    async fn reap_skips_controller() {
        let s = fresh_supervisor();
        let h = RunnerHandle::test_handle("g-controller", true);
        s.inner.runners.insert("g-controller".into(), h);
        let now = Utc::now() + chrono::Duration::seconds(IDLE_TTL.as_secs() as i64 * 2);
        let h = s.get("g-controller").unwrap();
        assert!(!h.is_reapable(now, IDLE_TTL).await);
    }

    #[tokio::test]
    async fn reap_skips_in_flight_runner() {
        let s = fresh_supervisor();
        let h = RunnerHandle::test_handle("g-1", false);
        h.state
            .write()
            .await
            .in_flight_turns
            .insert("turn-x".into());
        s.inner.runners.insert("g-1".into(), h);
        let h = s.get("g-1").unwrap();
        let now = Utc::now() + chrono::Duration::seconds(IDLE_TTL.as_secs() as i64 * 2);
        assert!(!h.is_reapable(now, IDLE_TTL).await);
    }

    #[tokio::test]
    async fn reap_drops_idle_non_controller() {
        let s = fresh_supervisor();
        let h = RunnerHandle::test_handle("g-1", false);
        // Force last_active_at into the past beyond TTL.
        {
            let mut state = h.state.write().await;
            state.last_active_at =
                Utc::now() - chrono::Duration::seconds(IDLE_TTL.as_secs() as i64 * 2);
        }
        s.inner.runners.insert("g-1".into(), h.clone());
        let now = Utc::now();
        assert!(h.is_reapable(now, IDLE_TTL).await);
    }

    #[tokio::test]
    async fn reap_group_protects_controller_against_idle() {
        let s = fresh_supervisor();
        let h = RunnerHandle::test_handle("g-controller", true);
        s.inner.runners.insert("g-controller".into(), h);
        let result = s
            .reap_group("g-controller", ShutdownReason::IdleReap)
            .await;
        assert!(matches!(result, Err(ReapError::ControllerProtected)));
    }

    #[tokio::test]
    async fn reap_group_allows_operator_wipe_on_controller() {
        // OperatorWipe is an explicit override — controllers ARE
        // wipeable on operator command. (They're just not
        // idle-reapable.)
        let s = fresh_supervisor();
        let h = RunnerHandle::test_handle("g-controller", true);
        s.inner.runners.insert("g-controller".into(), h);
        let result = s
            .reap_group("g-controller", ShutdownReason::OperatorWipe)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn forward_turn_errors_when_no_runner() {
        let s = fresh_supervisor();
        let req = TurnRequest {
            turn_id: "t".into(),
            conversation_id: "c".into(),
            group_id: "g-missing".into(),
            user_text: "hi".into(),
            sender_principal_id: "x".into(),
            sender_trust_class: "Controller".into(),
            system_prompt: "".into(),
            history: vec![],
            tool_catalog: vec![],
            inference_url: "http://infer".into(),
            model: "m".into(),
            temperature: None,
            max_tokens: None,
            reasoning_enabled: false,
            spotlight: None,
        };
        let res = s.forward_turn("g-missing", req).await;
        assert!(matches!(res, Err(ForwardError::NoRunner)));
    }

    #[tokio::test]
    async fn handle_inbound_token_delta_publishes_to_event_bus_and_stream() {
        let s = fresh_supervisor();
        let mut bus_rx = s.inner.events.subscribe();
        // Register a handle + attach an outbound channel.
        let (out_tx, _out_rx) = mpsc::unbounded_channel::<ServerToRunner>();
        let h = RunnerHandle::test_handle("g-1", true);
        *h.tx.lock().await = Some(out_tx);
        s.inner.runners.insert("g-1".into(), h.clone());
        // Set up the per-turn stream.
        let (turn_tx, mut turn_rx) = mpsc::unbounded_channel::<TurnEvent>();
        h.turn_streams.insert("t-1".into(), turn_tx);
        s.handle_inbound(
            "g-1",
            RunnerToServer::TokenDelta {
                turn_id: "t-1".into(),
                conversation_id: "conv-x".into(),
                text: "hello".into(),
            },
        )
        .await;
        // Per-turn stream got it.
        match turn_rx.try_recv().unwrap() {
            TurnEvent::TokenDelta { text } => assert_eq!(text, "hello"),
            other => panic!("unexpected: {other:?}"),
        }
        // EventBus too.
        let bus_msg = bus_rx.try_recv().unwrap();
        match bus_msg {
            UiEvent::ChatTokenDelta {
                conversation_id,
                text,
            } => {
                assert_eq!(conversation_id, "conv-x");
                assert_eq!(text, "hello");
            }
            _ => panic!("wrong UiEvent"),
        }
    }

    #[tokio::test]
    async fn turn_complete_decrements_in_flight_and_advances_last_active() {
        let s = fresh_supervisor();
        let h = RunnerHandle::test_handle("g-1", false);
        s.inner.runners.insert("g-1".into(), h.clone());
        // Set up an in-flight turn.
        let (turn_tx, mut turn_rx) = mpsc::unbounded_channel::<TurnEvent>();
        h.turn_streams.insert("t-1".into(), turn_tx);
        {
            let mut state = h.state.write().await;
            state.in_flight_turns.insert("t-1".into());
            state.last_active_at = Utc::now() - chrono::Duration::hours(1);
        }
        let before_active = h.state.read().await.last_active_at;
        s.handle_inbound(
            "g-1",
            RunnerToServer::TurnComplete {
                turn_id: "t-1".into(),
                conversation_id: "c-x".into(),
                assistant_text: "done".into(),
                finish_reason: Some("stop".into()),
                prompt_tokens: None,
                completion_tokens: None,
            },
        )
        .await;
        match turn_rx.recv().await.unwrap() {
            TurnEvent::Complete { .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
        let state = h.state.read().await;
        assert!(!state.in_flight_turns.contains("t-1"));
        assert!(state.last_active_at > before_active);
    }

    #[tokio::test]
    async fn drop_registration_fails_in_flight_turns() {
        let s = fresh_supervisor();
        let h = RunnerHandle::test_handle("g-1", false);
        let (turn_tx, mut turn_rx) = mpsc::unbounded_channel::<TurnEvent>();
        h.turn_streams.insert("t-1".into(), turn_tx);
        {
            let mut state = h.state.write().await;
            state.in_flight_turns.insert("t-1".into());
        }
        s.inner.runners.insert("g-1".into(), h);
        s.drop_registration("g-1").await;
        // The pending turn channel got an Error frame.
        match turn_rx.recv().await.unwrap() {
            TurnEvent::Error {
                message,
                cancelled,
            } => {
                assert!(message.contains("disconnected"));
                assert!(!cancelled);
            }
            other => panic!("unexpected: {other:?}"),
        }
        // Registry entry is gone.
        assert!(s.get("g-1").is_none());
    }

    #[tokio::test]
    async fn boot_orphan_sweep_wipes_only_unknown_volumes() {
        use crate::runner_spawn::{MockRunnerLauncher, RunnerLauncher, RunnerSpec};
        let s = fresh_supervisor();
        // Seed a known group via the principal-group store.
        let store = PrincipalGroupStore::new(&s.inner.db);
        let known = store
            .resolve(
                &execlaw_core::principal_groups::GroupKey {
                    channel: "web",
                    native_group_id: None,
                    principals: &[execlaw_core::ids::PrincipalId::from("controller")],
                    includes_controller: true,
                },
                1000,
            )
            .unwrap();

        let launcher = MockRunnerLauncher::new();
        // Spawn one volume for the known group, plus two orphans.
        let _ = launcher
            .spawn(&RunnerSpec {
                group_id: known.group_id.clone(),
                image: "x".into(),
                spawn_secret_hex: "00".into(),
                rpc_url: "ws://x".into(),
                inference_url: "http://x".into(),
                memory_bytes: None,
                network: None,
                env: vec![],
            })
            .await
            .unwrap();
        launcher.seed_volume("execlaw-runner-orphan-1").await;
        launcher.seed_volume("execlaw-runner-orphan-2").await;

        let wiped = s.boot_orphan_sweep(&launcher).await;
        assert_eq!(wiped.len(), 2);
        assert!(wiped.iter().any(|g| g == "orphan-1"));
        assert!(wiped.iter().any(|g| g == "orphan-2"));
        // The known group's volume was preserved.
        let after = launcher.list_runner_volumes().await.unwrap();
        assert!(
            after.iter().any(|v| v == &format!("execlaw-runner-{}", known.group_id))
        );
        assert!(!after.iter().any(|v| v == "execlaw-runner-orphan-1"));
    }

    #[tokio::test]
    async fn ensure_runner_returns_existing_when_already_registered() {
        use crate::runner_spawn::{MockRunnerLauncher, RunnerSpec};
        let s = fresh_supervisor();
        let launcher = MockRunnerLauncher::new();
        // Pre-register a runner directly.
        let (sec, _) = s.register_pending_spawn("g-1");
        let _h = s.accept_registration("g-1", &sec, false).unwrap();
        // ensure_runner returns the existing handle without spawning.
        let h = s
            .ensure_runner(
                &launcher,
                "g-1",
                RunnerSpec {
                    group_id: "g-1".into(),
                    image: "x".into(),
                    spawn_secret_hex: "".into(),
                    rpc_url: "ws://x".into(),
                    inference_url: "http://x".into(),
                    memory_bytes: None,
                    network: None,
                    env: vec![],
                },
                Duration::from_millis(50),
            )
            .await
            .unwrap();
        assert_eq!(h.group_id, "g-1");
        assert_eq!(launcher.spawn_count().await, 0);
    }

    /// 2026-04-28 — A handle stuck in `Stopping` (operator clicked
    /// Wipe / Restart but the runner never ack'd the Shutdown frame)
    /// is a tombstone, not a live runner. `ensure_runner` must drop
    /// the stale entry and spawn a fresh one — otherwise the chat
    /// path keeps getting handed a runner whose tx channel goes
    /// nowhere, and the operator has no escape from the SPA.
    #[tokio::test]
    async fn ensure_runner_drops_stopping_tombstone_and_respawns() {
        use crate::runner_spawn::{MockRunnerLauncher, RunnerSpec};
        let s = fresh_supervisor();
        let launcher = MockRunnerLauncher::new();
        // Seed a runner, then wedge it in Stopping.
        let (sec, _) = s.register_pending_spawn("g-stuck");
        let h = s.accept_registration("g-stuck", &sec, false).unwrap();
        {
            let mut state = h.state.write().await;
            state.status = RunnerStatus::Stopping;
        }
        // Pre-arm the launcher so the spawn future resolves + a
        // matching pending registration completes.
        let spec = RunnerSpec {
            group_id: "g-stuck".into(),
            image: "x".into(),
            spawn_secret_hex: "".into(),
            rpc_url: "ws://x".into(),
            inference_url: "http://x".into(),
            memory_bytes: None,
            network: None,
            env: vec![],
        };
        // Race: ensure_runner will mint a fresh secret + call
        // launcher.spawn(); we need to ack the registration on a
        // background task so ensure_runner's await unblocks.
        let s_clone = s.clone();
        let ack = tokio::spawn(async move {
            // Poll until ensure_runner posted a pending spawn.
            for _ in 0..50 {
                let secret = s_clone
                    .inner
                    .pending_spawns
                    .get("g-stuck")
                    .map(|p| p.value().secret);
                if let Some(secret) = secret {
                    let _ = s_clone.accept_registration("g-stuck", &secret, false);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let h2 = s
            .ensure_runner(&launcher, "g-stuck", spec, Duration::from_secs(2))
            .await
            .expect("ensure_runner should respawn over the tombstone");
        ack.await.unwrap();
        let new_status = h2.state.read().await.status;
        assert!(
            matches!(new_status, RunnerStatus::Spawning | RunnerStatus::Ready),
            "fresh respawn should be Spawning/Ready, got {new_status:?}"
        );
        assert_eq!(
            launcher.spawn_count().await,
            1,
            "stale tombstone should have been replaced via a single fresh spawn"
        );
    }

    #[tokio::test]
    async fn ensure_runner_times_out_when_runner_never_registers() {
        use crate::runner_spawn::{MockRunnerLauncher, RunnerSpec};
        let s = fresh_supervisor();
        let launcher = MockRunnerLauncher::new();
        let res = s
            .ensure_runner(
                &launcher,
                "g-no-runner",
                RunnerSpec {
                    group_id: "g-no-runner".into(),
                    image: "x".into(),
                    spawn_secret_hex: "".into(),
                    rpc_url: "ws://x".into(),
                    inference_url: "http://x".into(),
                    memory_bytes: None,
                    network: None,
                    env: vec![],
                },
                Duration::from_millis(50),
            )
            .await;
        assert!(matches!(res, Err(EnsureError::Timeout)));
        // Mock spawn was called once.
        assert_eq!(launcher.spawn_count().await, 1);
        // Container was killed on timeout (cleanup).
        let killed = launcher.killed().await;
        assert_eq!(killed.len(), 1);
        // Pending spawn entry was cleared.
        assert!(s.inner.pending_spawns.get("g-no-runner").is_none());
    }

    #[tokio::test]
    async fn reap_runner_idle_kills_container_and_wipes_volume() {
        use crate::runner_spawn::{MockRunnerLauncher, RunnerLauncher, RunnerSpec};
        let s = fresh_supervisor();
        let launcher = MockRunnerLauncher::new();
        let id = launcher
            .spawn(&RunnerSpec {
                group_id: "g-1".into(),
                image: "x".into(),
                spawn_secret_hex: "00".into(),
                rpc_url: "ws://x".into(),
                inference_url: "http://x".into(),
                memory_bytes: None,
                network: None,
                env: vec![],
            })
            .await
            .unwrap();
        // Manually build a registry entry pointing at that container.
        let (sec, _) = s.register_pending_spawn("g-1");
        let h = s.accept_registration("g-1", &sec, false).unwrap();
        h.state.write().await.container_id = Some(id.container_id.clone());
        // Attach a noop outbound channel so reap can send Shutdown.
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        *h.tx.lock().await = Some(out_tx);

        let report = s
            .reap_runner(&launcher, "g-1", ShutdownReason::IdleReap)
            .await
            .unwrap();
        assert!(report.wiped_volume, "idle reap must wipe volume");
        assert_eq!(launcher.killed().await, vec![id.container_id]);
        assert_eq!(launcher.wiped().await, vec!["execlaw-runner-g-1"]);
        assert!(s.get("g-1").is_none(), "registry entry dropped");
    }

    #[tokio::test]
    async fn reap_runner_operator_restart_preserves_volume() {
        use crate::runner_spawn::{MockRunnerLauncher, RunnerLauncher, RunnerSpec};
        let s = fresh_supervisor();
        let launcher = MockRunnerLauncher::new();
        let id = launcher
            .spawn(&RunnerSpec {
                group_id: "g-1".into(),
                image: "x".into(),
                spawn_secret_hex: "00".into(),
                rpc_url: "ws://x".into(),
                inference_url: "http://x".into(),
                memory_bytes: None,
                network: None,
                env: vec![],
            })
            .await
            .unwrap();
        let (sec, _) = s.register_pending_spawn("g-1");
        let h = s.accept_registration("g-1", &sec, false).unwrap();
        h.state.write().await.container_id = Some(id.container_id.clone());
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        *h.tx.lock().await = Some(out_tx);

        let report = s
            .reap_runner(&launcher, "g-1", ShutdownReason::OperatorRestart)
            .await
            .unwrap();
        assert!(!report.wiped_volume, "operator restart preserves volume");
        // Container WAS killed (it's a restart, not a no-op).
        assert_eq!(launcher.killed().await, vec![id.container_id]);
        // Volume wasn't wiped — restart preserves the workspace.
        assert!(launcher.wiped().await.is_empty());
    }

    #[tokio::test]
    async fn reap_runner_blocks_controller_idle_path() {
        use crate::runner_spawn::MockRunnerLauncher;
        let s = fresh_supervisor();
        let launcher = MockRunnerLauncher::new();
        let h = RunnerHandle::test_handle("g-controller", true);
        s.inner.runners.insert("g-controller".into(), h);
        let res = s
            .reap_runner(&launcher, "g-controller", ShutdownReason::IdleReap)
            .await;
        assert!(matches!(res, Err(ReapError::ControllerProtected)));
    }

    #[tokio::test]
    async fn reap_runner_operator_wipe_works_on_controller() {
        use crate::runner_spawn::{MockRunnerLauncher, RunnerLauncher, RunnerSpec};
        // Operator-driven wipe is an explicit override — works
        // even on the controller's runner.
        let s = fresh_supervisor();
        let launcher = MockRunnerLauncher::new();
        let id = launcher
            .spawn(&RunnerSpec {
                group_id: "g-controller".into(),
                image: "x".into(),
                spawn_secret_hex: "00".into(),
                rpc_url: "ws://x".into(),
                inference_url: "http://x".into(),
                memory_bytes: None,
                network: None,
                env: vec![],
            })
            .await
            .unwrap();
        let (sec, _) = s.register_pending_spawn("g-controller");
        let h = s.accept_registration("g-controller", &sec, true).unwrap();
        h.state.write().await.container_id = Some(id.container_id.clone());
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        *h.tx.lock().await = Some(out_tx);

        let report = s
            .reap_runner(&launcher, "g-controller", ShutdownReason::OperatorWipe)
            .await
            .unwrap();
        assert!(report.wiped_volume);
    }

    #[tokio::test]
    async fn boot_orphan_sweep_skips_non_runner_volumes() {
        use crate::runner_spawn::{MockRunnerLauncher, RunnerLauncher};
        let s = fresh_supervisor();
        let launcher = MockRunnerLauncher::new();
        // A volume that doesn't match the runner prefix should be
        // ignored (e.g. an unrelated bind mount the operator
        // happens to have).
        launcher.seed_volume("postgres-data").await;
        let wiped = s.boot_orphan_sweep(&launcher).await;
        assert!(wiped.is_empty());
    }

    #[tokio::test]
    async fn watchdog_cancels_only_overdue_turns() {
        let s = fresh_supervisor();
        let h = RunnerHandle::test_handle("g-1", false);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerToRunner>();
        *h.tx.lock().await = Some(out_tx);
        let now = Utc::now();
        {
            let mut state = h.state.write().await;
            state
                .turn_deadlines
                .insert("t-overdue".into(), now - chrono::Duration::seconds(5));
            state
                .turn_deadlines
                .insert("t-fresh".into(), now + chrono::Duration::hours(1));
        }
        s.inner.runners.insert("g-1".into(), h);

        s.watchdog_pass().await;
        let frame = out_rx.try_recv().unwrap();
        match frame {
            ServerToRunner::CancelTurn { turn_id } => assert_eq!(turn_id, "t-overdue"),
            other => panic!("unexpected: {other:?}"),
        }
        // Only one cancel — the fresh turn was untouched.
        assert!(out_rx.try_recv().is_err());
    }
}
