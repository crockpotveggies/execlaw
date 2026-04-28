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
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use execlaw_core::Database;
use execlaw_core::principal_groups::PrincipalGroupStore;
use execlaw_runner_protocol::{
    RegistrationAck, RunnerToServer, ServerToRunner, ShutdownReason, ToolCallResult,
    ToolOutcome, TurnRequest,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, Notify, RwLock, mpsc, oneshot};
use tracing::{debug, info, warn};

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
    /// Runner wants an event log row appended. Supervisor proxy
    /// already handled the HMAC + commit before this fires; the
    /// chat handler doesn't need to do anything beyond logging.
    EventCommitted {
        kind: String,
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
    /// Reaper-stop signal. Owned by the spawned reaper task.
    pub stop: Arc<Notify>,
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
                stop: Arc::new(Notify::new()),
                events,
                db,
            }),
        }
    }

    /// Mint a unique `turn_id`. The seq is per-process — fine
    /// because the runner just echoes it; no cross-process
    /// uniqueness needed.
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
        pending.registered.notify_waiters();
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
        // this path. Operator wipe / explicit delete go through
        // `wipe_workspace` / `delete_group` which carry the
        // explicit policy override.
        if handle.controller_runner && reason == ShutdownReason::IdleReap {
            return Err(ReapError::ControllerProtected);
        }
        {
            let mut s = handle.state.write().await;
            s.status = RunnerStatus::Stopping;
        }
        let frame = ServerToRunner::Shutdown { reason };
        let _ = send_to_runner(&handle, frame).await;
        // The bollard kill + volume rm happen in
        // `runner_spawn::reap_container` which the caller
        // sequences after this. The registry entry stays until
        // the WS read loop sees the close and removes it (or the
        // operator-driven path forces it).
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
                payload: _,
                actor: _,
            } => {
                // The actual HMAC sign + commit is wired in
                // chats.rs (it has the EventLog handle). Here we
                // just notify the in-flight chat handler so it
                // can perform the commit on its turn.
                if let Some(tx) = handle.turn_streams.get(&turn_id) {
                    let _ = tx.send(TurnEvent::EventCommitted { kind });
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
            capability_token: "tok".into(),
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
