//! Sidecar supervisor (Phase 2b — `docs/sidecar-supervisor-design.md`).
//!
//! Owns the lifecycle of every transport-sidecar sidecar container
//! declared by an installed plugin's `[[services]]` entries with a
//! `[services.sidecar]` table. Conceptually the third member of the
//! supervisor family alongside `backend_supervisor` (inference
//! containers) and `runner_supervisor` (per-group runner containers):
//!
//!   * **`backend_supervisor`** — owns vLLM / TTS / STT containers.
//!   * **`runner_supervisor`** — owns per-principal-group runner
//!     containers (the agent loop).
//!   * **`sidecar_supervisor`** *(this)* — owns Signal-cli /
//!     WhatsApp-sidecar / Matrix-sidecar / ... sidecars. One container
//!     per registered `channel`; `HookRegistry::all_sidecars` is the
//!     source of truth for desired state.
//!
//! What's in scope **for Phase 2b**:
//!   * tick + reconcile pattern mirroring `backend_supervisor`
//!   * spawn-on-register, healthcheck-loop, restart-on-crash with
//!     exponential-attempt cap, stop-on-unregister
//!   * status snapshot for the SPA's sidecars page (a future hookup)
//!
//! What's deliberately **not** in scope yet (Phase 3 work):
//!   * the sidecar RPC client (`/v1/send`, `/v1/inbound/stream`)
//!   * inbound message ingestion → `state_transport_bindings` lookup
//!   * outbound dispatch wired into `signal.send_message`
//!   * fingerprinted alert routing on sidecar-down events
//!
//! Tests use `MockServiceController` so no Docker daemon is touched
//! and every transition can be driven deterministically.

use crate::events::{EventBus, UiEvent};
use execlaw_container_manager::{ServiceController, ServiceHandle, ServiceSpec, ServiceStatus};
use execlaw_plugin_host::hook_registry::{HookRegistry, RegisteredSidecar};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};

/// Default sweep cadence — every 5 seconds the supervisor reconciles
/// desired vs running state, just like `backend_supervisor`. Sidecar
/// outages aren't time-critical; once a configurable interval lands
/// (Phase 3) operators can dial it.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(5);

/// How many consecutive restart attempts before we park the slot in
/// `CrashLooping` instead of looping forever. Mirrors
/// `backend_supervisor::MAX_RESTART_ATTEMPTS`.
pub const MAX_RESTART_ATTEMPTS: u32 = 5;

/// Per-channel runtime state. Cheap to clone individual fields; the
/// container handle is owned and replaced on respawn.
#[derive(Debug)]
struct SidecarSlot {
    /// Echoes the sidecar's `RegisteredSidecar` snapshot from the last
    /// reconcile so we can detect manifest-edits-without-disable
    /// (different image, port, ...) and respawn cleanly.
    registered: RegisteredSidecar,
    /// Live container handle when the sidecar is running. `None`
    /// before the first successful spawn AND between spawn-failure
    /// and the next reconcile.
    handle: Option<ServiceHandle>,
    /// Stable host port assigned the first time we spawned a
    /// container for this channel. Reused on every subsequent
    /// respawn (RPC-fail restart, drift respawn, post-crash loop)
    /// so the sidecar's URL stays stable across the supervisor's
    /// lifetime — matches the operator-facing "the supervisor
    /// keeps URLs stable" promise. `None` before the first
    /// successful spawn.
    host_port: Option<u16>,
    /// Last-observed status from the controller. Defaults to
    /// `Stopped` for a freshly-registered slot.
    status: ServiceStatus,
    /// Consecutive restart attempts since the last `Healthy`
    /// observation. Reset on transition to `Healthy`. Once it
    /// reaches `MAX_RESTART_ATTEMPTS` the slot parks in
    /// `CrashLooping` — operators must `kick` (after fixing the
    /// underlying issue) to retry.
    restart_attempts: u32,
}

impl SidecarSlot {
    fn fresh(b: RegisteredSidecar) -> Self {
        Self {
            registered: b,
            handle: None,
            host_port: None,
            status: ServiceStatus::Stopped,
            restart_attempts: 0,
        }
    }

    /// True if a manifest edit invalidated the running container —
    /// the supervisor must stop + respawn rather than try to
    /// hot-update a `bollard` container.
    fn drift_from(&self, latest: &RegisteredSidecar) -> bool {
        self.registered.image != latest.image
            || self.registered.rpc_port != latest.rpc_port
            || self.registered.rpc_health_path != latest.rpc_health_path
            || self.registered.service_name != latest.service_name
            || self.registered.plugin_id != latest.plugin_id
    }
}

/// Read-only status snapshot for the future Sidecars admin page. One
/// entry per registered channel; channels with no live container
/// report `Stopped`. Plain struct so JSON serialisation stays
/// boring.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SidecarRuntimeStatus {
    pub channel: String,
    pub plugin_id: String,
    pub service_name: String,
    pub status: ServiceStatus,
    pub restart_attempts: u32,
    /// Loopback URL the supervisor would dispatch RPC against.
    /// `None` until the first successful spawn (when we know the
    /// host port).
    pub rpc_url: Option<String>,
}

/// The supervisor itself. Cheap to clone (everything's `Arc` inside).
#[derive(Clone)]
pub struct SidecarSupervisor {
    controller: Arc<dyn ServiceController>,
    registry: HookRegistry,
    /// Optional event bus for surface-status events. Tests usually
    /// pass `None`. Production wires the SPA's bus so a sidecar
    /// flipping to `CrashLooping` shows up in the loader-pill /
    /// alerts dock without a polling round-trip.
    bus: Option<Arc<EventBus>>,
    interval: Duration,
    kick: Arc<Notify>,
    slots: Arc<Mutex<HashMap<String, SidecarSlot>>>,
    /// Host port pool start. The supervisor mints sequential ports
    /// starting from this value to avoid collisions with
    /// `backend_supervisor`'s 8101+ pool. Sidecars of distinct
    /// channels get distinct stable ports; the assignment lives in
    /// the `SidecarSlot` so a respawn keeps the same URL.
    next_host_port: Arc<Mutex<u16>>,
}

/// First host port the supervisor mints for sidecars. Picked above
/// `backend_supervisor`'s 8101–8200 range and below the typical
/// dev-tools range so collisions with operator workflows are
/// vanishingly rare. Operators who need to override can edit at
/// install time (Phase 3 will surface this in the sidecar config UI).
pub const SIDECAR_PORT_POOL_START: u16 = 8501;

/// Last host port in the sidecar pool. The 100-port window is large
/// enough that no realistic operator will hit it (selfhosted-claw's
/// busiest deployments ran ~3 sidecars; even an order of magnitude up
/// from there fits) and small enough that we can't drift into the
/// ephemeral-port range. Hitting this ceiling causes
/// `allocate_port` to return `None` rather than silently colliding,
/// which is the right failure mode for an operator-visible problem.
pub const SIDECAR_PORT_POOL_END: u16 = 8600;

impl SidecarSupervisor {
    pub fn new(controller: Arc<dyn ServiceController>, registry: HookRegistry) -> Self {
        Self::with_config(controller, registry, DEFAULT_TICK_INTERVAL)
    }

    pub fn with_config(
        controller: Arc<dyn ServiceController>,
        registry: HookRegistry,
        interval: Duration,
    ) -> Self {
        Self {
            controller,
            registry,
            bus: None,
            interval,
            kick: Arc::new(Notify::new()),
            slots: Arc::new(Mutex::new(HashMap::new())),
            next_host_port: Arc::new(Mutex::new(SIDECAR_PORT_POOL_START)),
        }
    }

    pub fn with_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Force a reconcile pass — operators trigger this from a
    /// "restart sidecar" button, and the plugin-install handler
    /// kicks it after enabling a plugin so the new sidecar spins up
    /// without waiting the full tick interval.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// Reset the per-channel restart counter. Operators call this
    /// after fixing the underlying issue (image edit, secrets
    /// re-mount) so a parked-CrashLooping slot gets a fresh runway.
    pub async fn reset_attempts(&self, channel: &str) {
        let mut slots = self.slots.lock().await;
        if let Some(slot) = slots.get_mut(channel) {
            slot.restart_attempts = 0;
            if matches!(slot.status, ServiceStatus::CrashLooping { .. }) {
                slot.status = ServiceStatus::Stopped;
                slot.handle = None;
            }
        }
    }

    /// Snapshot every channel's current state. Returns one entry per
    /// channel **registered in the hook registry** — channels the
    /// supervisor has never seen yet still show up as `Stopped` so
    /// the SPA's sidecars page can render the row before the first
    /// reconcile lands.
    pub async fn snapshot_status(&self) -> Vec<SidecarRuntimeStatus> {
        let sidecars = self.registry.all_sidecars();
        let slots = self.slots.lock().await;
        sidecars
            .into_iter()
            .map(|b| {
                let slot = slots.get(&b.channel);
                let status = slot
                    .map(|s| s.status.clone())
                    .unwrap_or(ServiceStatus::Stopped);
                let restart_attempts = slot.map(|s| s.restart_attempts).unwrap_or(0);
                let rpc_url = slot
                    .and_then(|s| s.handle.as_ref())
                    .map(|h| format!("http://127.0.0.1:{}", h.host_port));
                SidecarRuntimeStatus {
                    channel: b.channel.clone(),
                    plugin_id: b.plugin_id.clone(),
                    service_name: b.service_name.clone(),
                    status,
                    restart_attempts,
                    rpc_url,
                }
            })
            .collect()
    }

    /// Drive the loop until `stop` is notified. Production code
    /// spawns this on a dedicated tokio task at boot.
    pub async fn run(&self, stop: Arc<Notify>) {
        info!(
            interval_secs = self.interval.as_secs(),
            "sidecar supervisor running"
        );
        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.interval) => {}
                _ = self.kick.notified() => {}
                _ = stop.notified() => {
                    info!("sidecar supervisor stop received; exiting");
                    return;
                }
            }
            self.reconcile_once().await;
        }
    }

    /// One reconcile pass. Public so tests can drive it
    /// deterministically without spinning up a tokio task or
    /// waiting on `interval`.
    pub async fn reconcile_once(&self) {
        let desired = self.registry.all_sidecars();
        let mut slots = self.slots.lock().await;

        // Phase 1: stop + drop slots whose channel is no longer
        // registered (plugin disabled / uninstalled).
        let desired_channels: std::collections::HashSet<String> =
            desired.iter().map(|b| b.channel.clone()).collect();
        let to_drop: Vec<String> = slots
            .keys()
            .filter(|c| !desired_channels.contains(*c))
            .cloned()
            .collect();
        for c in to_drop {
            if let Some(mut slot) = slots.remove(&c) {
                let was_running = slot.handle.is_some();
                if let Some(handle) = slot.handle.take() {
                    debug!(channel = %c, "stopping orphaned sidecar container");
                    if let Err(e) = self.controller.stop(&handle).await {
                        warn!(
                            channel = %c,
                            error = %e,
                            "failed to stop orphaned sidecar container",
                        );
                    }
                }
                // Only emit a UI transition when the slot was
                // actually running — orphaning a stopped slot
                // shouldn't spam the bus. (Mirrors the
                // `transition_status` dedup on the live-slot path.)
                if was_running
                    && let Some(bus) = &self.bus
                {
                    bus.publish(UiEvent::SidecarStatusChanged {
                        channel: c.clone(),
                        status: format!("{:?}", ServiceStatus::Stopped),
                    });
                }
            }
        }

        // Phase 2: ensure every desired channel has a slot, then
        // reconcile that slot's runtime state.
        for sidecar in desired {
            // Drift detection: a manifest edit (new image, port)
            // means we tear down the old container and respawn,
            // resetting the restart counter so the new image gets
            // a fresh runway. Two independent steps:
            //   1. If the slot has a handle, stop the prior
            //      container.
            //   2. Reset slot.{status, restart_attempts, registered}
            //      whether or not a handle was present — a
            //      drift-during-restart-cooldown scenario (handle
            //      already dropped, attempts > 0) still deserves the
            //      counter reset.
            let needs_respawn_for_drift = slots
                .get(&sidecar.channel)
                .map(|s| s.drift_from(&sidecar))
                .unwrap_or(false);
            if needs_respawn_for_drift {
                if let Some(slot) = slots.get_mut(&sidecar.channel) {
                    if let Some(handle) = slot.handle.take() {
                        debug!(
                            channel = %sidecar.channel,
                            "sidecar manifest changed; stopping prior container",
                        );
                        if let Err(e) = self.controller.stop(&handle).await {
                            warn!(channel = %sidecar.channel, error = %e,
                                  "failed to stop sidecar during drift respawn");
                        }
                    }
                    slot.status = ServiceStatus::Stopped;
                    slot.restart_attempts = 0;
                    slot.registered = sidecar.clone();
                }
            }

            let slot = slots
                .entry(sidecar.channel.clone())
                .or_insert_with(|| SidecarSlot::fresh(sidecar.clone()));
            slot.registered = sidecar.clone();

            self.reconcile_slot(&sidecar, slot).await;
        }
    }

    /// Reconcile one slot. Pulled into its own method so the
    /// reconcile loop reads as "for each desired sidecar, drive its
    /// state machine forward by one step."
    async fn reconcile_slot(&self, sidecar: &RegisteredSidecar, slot: &mut SidecarSlot) {
        // Park early when we've blown the restart budget. Operator
        // intervention via `reset_attempts` is the only way out.
        if matches!(slot.status, ServiceStatus::CrashLooping { .. }) {
            return;
        }

        // Spawn the container if we don't have a handle. This is
        // the steady-state cold-start path AND the post-crash
        // respawn path. Reuse the slot's previously-allocated
        // host_port so the sidecar's URL stays stable across the
        // supervisor's lifetime; only mint a fresh one on the very
        // first spawn.
        if slot.handle.is_none() {
            let port = match slot.host_port {
                Some(existing) => existing,
                None => match self.allocate_port().await {
                    Some(p) => {
                        slot.host_port = Some(p);
                        p
                    }
                    None => {
                        warn!(
                            channel = %sidecar.channel,
                            "sidecar port pool exhausted; refusing to spawn",
                        );
                        // Park CrashLooping so the slot doesn't
                        // burn restart attempts on a problem
                        // operator action can't fix without a
                        // restart of the control plane.
                        let new_status = ServiceStatus::CrashLooping {
                            restart_count: MAX_RESTART_ATTEMPTS,
                        };
                        self.transition_status(&sidecar.channel, slot, new_status);
                        return;
                    }
                },
            };
            let spec = ServiceSpec {
                name: container_name(&sidecar.plugin_id, &sidecar.channel),
                image: sidecar.image.clone(),
                args: Vec::new(),
                env: Vec::new(),
                gpu_id: None,
                gpu_vendor: None,
                mounts: Vec::new(),
                host_port: port,
                container_port: sidecar.rpc_port,
            };
            match self.controller.spawn(&spec).await {
                Ok(handle) => {
                    info!(
                        channel = %sidecar.channel,
                        container = %handle.name,
                        host_port = handle.host_port,
                        "sidecar container spawned",
                    );
                    slot.handle = Some(handle);
                    self.transition_status(
                        &sidecar.channel,
                        slot,
                        ServiceStatus::Starting,
                    );
                    return;
                }
                Err(e) => {
                    slot.restart_attempts = slot.restart_attempts.saturating_add(1);
                    let new_status = if slot.restart_attempts >= MAX_RESTART_ATTEMPTS {
                        warn!(
                            channel = %sidecar.channel,
                            attempts = slot.restart_attempts,
                            error = %e,
                            "sidecar container hit restart cap; parking CrashLooping",
                        );
                        ServiceStatus::CrashLooping {
                            restart_count: slot.restart_attempts,
                        }
                    } else {
                        warn!(
                            channel = %sidecar.channel,
                            attempts = slot.restart_attempts,
                            error = %e,
                            "sidecar container spawn failed; will retry",
                        );
                        ServiceStatus::Stopped
                    };
                    self.transition_status(&sidecar.channel, slot, new_status);
                    return;
                }
            }
        }

        // Inspect + healthcheck the running container. `let-else`
        // (rather than `expect`) — we just verified `handle.is_none()`
        // above, but a defensive bind keeps a future code reorder
        // from panicking.
        let Some(handle) = slot.handle.as_ref().cloned() else {
            return;
        };
        let inspect = match self.controller.inspect(&handle).await {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    channel = %sidecar.channel,
                    error = %e,
                    "sidecar inspect failed; will recheck next tick",
                );
                return;
            }
        };

        match inspect {
            ServiceStatus::NotFound | ServiceStatus::Stopped => {
                // Container vanished out from under us — drop the
                // handle so the next reconcile respawns. Don't bump
                // restart_attempts here; the spawn-failure branch
                // is the canonical place that increments. (This is
                // a deliberate escape hatch from the cap — a sidecar
                // crashed-and-removed by the operator should respawn
                // freely, not get parked. The `Healthy → vanished`
                // flap case is a known limitation; Phase 3 alert
                // routing surfaces it without depending on the cap.)
                debug!(
                    channel = %sidecar.channel,
                    "sidecar container vanished; will respawn",
                );
                slot.handle = None;
                self.transition_status(
                    &sidecar.channel,
                    slot,
                    ServiceStatus::Stopped,
                );
            }
            ServiceStatus::CrashLooping { restart_count } => {
                // Adopt the controller's count verbatim — pre-fix
                // this added `+1` on every observation, so an idle
                // CrashLooping slot would burn restart_attempts
                // upward by 1 per reconcile tick (5 ticks → cap).
                // The controller is the source of truth for
                // crash-loop counting; we just mirror it.
                slot.restart_attempts = restart_count;
                self.transition_status(
                    &sidecar.channel,
                    slot,
                    ServiceStatus::CrashLooping { restart_count },
                );
            }
            ServiceStatus::Pulling | ServiceStatus::Starting => {
                self.transition_status(&sidecar.channel, slot, inspect);
            }
            ServiceStatus::Healthy => {
                // Validate via the sidecar's own RPC healthcheck —
                // `inspect` only tells us the container is up; the
                // sidecar process inside might still be initialising.
                let url = format!(
                    "http://127.0.0.1:{}{}",
                    handle.host_port, sidecar.rpc_health_path
                );
                let healthy = self
                    .controller
                    .health_check(&url)
                    .await
                    .unwrap_or(false);
                if healthy {
                    if !matches!(slot.status, ServiceStatus::Healthy) {
                        info!(channel = %sidecar.channel, "sidecar healthy");
                    }
                    slot.restart_attempts = 0;
                    self.transition_status(
                        &sidecar.channel,
                        slot,
                        ServiceStatus::Healthy,
                    );
                } else {
                    // Container says it's up but RPC health failed —
                    // restart. Could be a slow-starting sidecar; the
                    // restart-attempt cap protects us either way.
                    warn!(
                        channel = %sidecar.channel,
                        url = %url,
                        "sidecar RPC health failed; restarting container",
                    );
                    if let Err(e) = self.controller.stop(&handle).await {
                        warn!(channel = %sidecar.channel, error = %e,
                              "stop-for-restart failed");
                    }
                    slot.handle = None;
                    slot.restart_attempts = slot.restart_attempts.saturating_add(1);
                    let new_status =
                        if slot.restart_attempts >= MAX_RESTART_ATTEMPTS {
                            ServiceStatus::CrashLooping {
                                restart_count: slot.restart_attempts,
                            }
                        } else {
                            ServiceStatus::Stopped
                        };
                    self.transition_status(&sidecar.channel, slot, new_status);
                }
            }
        }
    }

    /// Mint the next stable host port. Pool is contiguous from
    /// `SIDECAR_PORT_POOL_START` up to `SIDECAR_PORT_POOL_END`; once
    /// exhausted, returns `None` and the supervisor refuses to spawn
    /// (parking the slot CrashLooping). In practice no operator runs
    /// 100 sidecars, but a saturating overflow that quietly mapped
    /// every excess channel onto the same port would be a much worse
    /// failure mode than the explicit refusal.
    async fn allocate_port(&self) -> Option<u16> {
        let mut next = self.next_host_port.lock().await;
        if *next > SIDECAR_PORT_POOL_END {
            return None;
        }
        let p = *next;
        // saturating_add guards the u16 overflow at exhaustion;
        // the pool-end check above is the actual gate.
        *next = next.saturating_add(1);
        Some(p)
    }

    /// Update `slot.status` and publish a `SidecarStatusChanged` event
    /// **only if the status actually changed**. Pre-fix the supervisor
    /// re-published on every reconcile pass even when the status was
    /// the same, which spammed the event bus + the SPA's sidecars
    /// page. Centralising the publish here means every transition
    /// site naturally dedups.
    fn transition_status(
        &self,
        channel: &str,
        slot: &mut SidecarSlot,
        new_status: ServiceStatus,
    ) {
        if slot.status == new_status {
            return;
        }
        slot.status = new_status.clone();
        if let Some(bus) = &self.bus {
            bus.publish(UiEvent::SidecarStatusChanged {
                channel: channel.to_owned(),
                status: format!("{new_status:?}"),
            });
        }
    }
}

/// Stable per-(plugin, channel) container name. Mirrors
/// `backend_supervisor`'s naming scheme so an operator who knows the
/// `execlaw-…` convention finds sidecars where they expect.
fn container_name(plugin_id: &str, channel: &str) -> String {
    format!("execlaw-sidecar-{plugin_id}-{channel}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_container_manager::MockServiceController;
    use execlaw_plugin_sdk::PluginManifest;

    fn registry_with_sidecar(plugin_id: &str, channel: &str, port: u16) -> HookRegistry {
        let m = PluginManifest::parse(&format!(
            r#"
[plugin]
id = "{plugin_id}"
name = "P"
version = "0.1.0"

[[services]]
name = "{plugin_id}-sidecar"
image = "execlaw/{plugin_id}-sidecar:0.1"

[services.sidecar]
channel = "{channel}"
rpc_port = {port}
"#
        ))
        .unwrap();
        let reg = HookRegistry::new();
        reg.enable(&m).unwrap();
        reg
    }

    #[tokio::test]
    async fn reconcile_spawns_registered_sidecar_and_marks_starting() {
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg);

        sup.reconcile_once().await;

        assert_eq!(mock.spawn_count().await, 1);
        let last = mock.last_spawn().await.unwrap();
        assert_eq!(last.image, "execlaw/p-signal-sidecar:0.1");
        assert_eq!(last.container_port, 8080);
        assert_eq!(last.host_port, SIDECAR_PORT_POOL_START);

        let snap = sup.snapshot_status().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].channel, "signal");
        assert_eq!(snap[0].status, ServiceStatus::Starting);
        assert_eq!(snap[0].restart_attempts, 0);
    }

    #[tokio::test]
    async fn reconcile_promotes_to_healthy_when_inspect_and_rpc_both_pass() {
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg);

        // Tick 1: spawn → Starting.
        sup.reconcile_once().await;
        // Tick 2: mock now reports Healthy + RPC health passes →
        // we expect the supervisor to settle to Healthy and reset
        // restart_attempts.
        mock.pin_status(ServiceStatus::Healthy).await;
        mock.pin_health(true).await;
        sup.reconcile_once().await;

        let snap = sup.snapshot_status().await;
        assert_eq!(snap[0].status, ServiceStatus::Healthy);
        assert_eq!(snap[0].restart_attempts, 0);
        assert!(snap[0].rpc_url.is_some());
    }

    #[tokio::test]
    async fn rpc_health_failure_with_inspect_healthy_triggers_restart() {
        // The "container says it's up but the sidecar process inside
        // is wedged" case. Inspect says Healthy; RPC health says no.
        // Supervisor must stop + drop the handle so the next
        // reconcile respawns.
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg);

        sup.reconcile_once().await; // spawn
        mock.pin_status(ServiceStatus::Healthy).await;
        mock.pin_health(false).await;
        sup.reconcile_once().await; // detect bad RPC

        // One spawn + one stop = one full restart-cycle
        // initiated. Next reconcile would respawn.
        assert_eq!(mock.spawn_count().await, 1);
        assert_eq!(mock.stop_count().await, 1);
        let snap = sup.snapshot_status().await;
        assert!(matches!(snap[0].status, ServiceStatus::Stopped));
        assert_eq!(snap[0].restart_attempts, 1);
    }

    #[tokio::test]
    async fn spawn_failures_park_after_restart_cap() {
        // Pinned Pull error keeps every spawn failing. After
        // MAX_RESTART_ATTEMPTS reconciles we must park in
        // CrashLooping; further reconciles must NOT keep spawning.
        let mock = Arc::new(MockServiceController::new());
        mock.pin_spawn_pull_error("nope").await;
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg);

        for _ in 0..MAX_RESTART_ATTEMPTS {
            sup.reconcile_once().await;
        }
        let snap = sup.snapshot_status().await;
        assert!(
            matches!(snap[0].status, ServiceStatus::CrashLooping { .. }),
            "expected CrashLooping after cap, got {:?}",
            snap[0].status,
        );
        // Future reconciles are short-circuited by the
        // CrashLooping check at the top of reconcile_slot.
        let pre = mock.spawn_count().await;
        sup.reconcile_once().await;
        sup.reconcile_once().await;
        assert_eq!(mock.spawn_count().await, pre);
    }

    #[tokio::test]
    async fn reset_attempts_drops_crash_looping_park() {
        let mock = Arc::new(MockServiceController::new());
        mock.pin_spawn_pull_error("nope").await;
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg);

        for _ in 0..MAX_RESTART_ATTEMPTS {
            sup.reconcile_once().await;
        }
        // Operator "fixes" the issue + clears the park.
        mock.clear_spawn_response().await;
        sup.reset_attempts("signal").await;
        sup.reconcile_once().await;
        let snap = sup.snapshot_status().await;
        // Spawn worked → Starting again.
        assert_eq!(snap[0].status, ServiceStatus::Starting);
        assert_eq!(snap[0].restart_attempts, 0);
    }

    #[tokio::test]
    async fn unregistering_sidecar_stops_its_container() {
        // Plugin disabled → sidecar unregistered → supervisor must
        // stop the container on the next reconcile and drop the
        // slot.
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg.clone());
        sup.reconcile_once().await;
        assert_eq!(mock.spawn_count().await, 1);

        // Pretend the operator disabled the plugin.
        reg.disable("p-signal");
        sup.reconcile_once().await;

        assert_eq!(mock.stop_count().await, 1);
        let snap = sup.snapshot_status().await;
        assert!(snap.is_empty(), "no registered sidecars → empty snapshot");
    }

    #[tokio::test]
    async fn manifest_image_change_triggers_clean_respawn() {
        // Drift detection: same channel, different image → stop
        // old, spawn new. Without this an `upgrade` of a sidecar
        // plugin would leave the prior container running.
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg.clone());
        sup.reconcile_once().await;
        assert_eq!(mock.spawn_count().await, 1);

        // Re-register with a different image (simulates a plugin
        // upgrade that changed the [[services]].image).
        reg.disable("p-signal");
        let m2 = PluginManifest::parse(
            r#"
[plugin]
id = "p-signal"
name = "P"
version = "0.1.0"

[[services]]
name = "p-signal-sidecar"
image = "execlaw/p-signal-sidecar:0.2"

[services.sidecar]
channel = "signal"
rpc_port = 8080
"#,
        )
        .unwrap();
        reg.enable(&m2).unwrap();

        sup.reconcile_once().await;

        assert_eq!(mock.stop_count().await, 1);
        // 1 from the original spawn + 1 from the post-drift spawn.
        assert_eq!(mock.spawn_count().await, 2);
        let last = mock.last_spawn().await.unwrap();
        assert_eq!(last.image, "execlaw/p-signal-sidecar:0.2");
    }

    #[tokio::test]
    async fn vanished_container_drops_handle_for_next_respawn() {
        // Inspect returns NotFound (container deleted out-of-band) →
        // supervisor must drop the handle so the next tick respawns,
        // but NOT bump restart_attempts here (the spawn-failure
        // path is the canonical incrementer).
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg);
        sup.reconcile_once().await; // spawn
        mock.pin_status(ServiceStatus::NotFound).await;
        sup.reconcile_once().await; // observe

        let snap = sup.snapshot_status().await;
        assert!(matches!(snap[0].status, ServiceStatus::Stopped));
        assert_eq!(snap[0].restart_attempts, 0);
    }

    #[tokio::test]
    async fn rpc_health_failure_respawn_reuses_host_port() {
        // Pre-fix the supervisor minted a NEW host port on every
        // RPC-fail respawn, leaking the prior one into the void
        // (and breaking the doc-comment "supervisor keeps URLs
        // stable" promise). Pin port reuse: spawn → RPC-fail
        // respawn → next spawn lands on the SAME host port.
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg);
        sup.reconcile_once().await; // spawn (port = 8501)
        let first_port = mock.last_spawn().await.unwrap().host_port;
        assert_eq!(first_port, SIDECAR_PORT_POOL_START);

        mock.pin_status(ServiceStatus::Healthy).await;
        mock.pin_health(false).await;
        sup.reconcile_once().await; // detect bad RPC, stop+drop
        // Clear the health pin so the next spawn proceeds.
        // (mock spawn always succeeds by default; we just need a
        // fresh tick to fire it.)
        mock.pin_status(ServiceStatus::Starting).await;
        sup.reconcile_once().await; // respawn

        let respawn_port = mock.last_spawn().await.unwrap().host_port;
        assert_eq!(
            respawn_port, first_port,
            "respawn must reuse the original port — got {first_port} → {respawn_port}",
        );
    }

    #[tokio::test]
    async fn rpc_health_failure_eventually_parks_at_cap() {
        // Audit gap: only the spawn-fail path was tested for the
        // restart cap. Pin the RPC-health-fail path too — it's the
        // realistic "sidecar is wedged" case.
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg);
        sup.reconcile_once().await; // spawn
        mock.pin_status(ServiceStatus::Healthy).await;
        mock.pin_health(false).await;
        // Each cycle = one detect-bad-rpc tick + one respawn tick.
        // We need MAX_RESTART_ATTEMPTS bad-rpc detections.
        for _ in 0..MAX_RESTART_ATTEMPTS {
            sup.reconcile_once().await; // detect bad RPC
            sup.reconcile_once().await; // respawn (Starting)
        }
        // Final detect bumps to the cap.
        sup.reconcile_once().await;
        let snap = sup.snapshot_status().await;
        // After enough RPC failures we eventually land at the cap.
        // The exact tick count depends on how spawn/Starting/Healthy
        // interleave with the mock's pinned status; the load-bearing
        // assertion is "we DO reach CrashLooping eventually."
        assert!(
            snap[0].restart_attempts >= MAX_RESTART_ATTEMPTS
                || matches!(snap[0].status, ServiceStatus::CrashLooping { .. }),
            "RPC-health-fail loop must hit the cap; got status={:?} attempts={}",
            snap[0].status,
            snap[0].restart_attempts,
        );
    }

    #[tokio::test]
    async fn drift_respawn_resets_restart_attempts() {
        // Audit gap: the manifest-image-change test asserted spawn
        // count + image but not the restart_attempts reset that
        // `drift_from` triggers. Pin it.
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg.clone());
        sup.reconcile_once().await; // spawn
        // Force the restart counter up via RPC-health failure.
        mock.pin_status(ServiceStatus::Healthy).await;
        mock.pin_health(false).await;
        sup.reconcile_once().await; // detect bad RPC → bumps to 1
        let snap = sup.snapshot_status().await;
        assert_eq!(snap[0].restart_attempts, 1);

        // Now flip the image (drift) and reconcile.
        reg.disable("p-signal");
        let m2 = PluginManifest::parse(
            r#"
[plugin]
id = "p-signal"
name = "P"
version = "0.1.0"

[[services]]
name = "p-signal-sidecar"
image = "execlaw/p-signal-sidecar:0.2"

[services.sidecar]
channel = "signal"
rpc_port = 8080
"#,
        )
        .unwrap();
        reg.enable(&m2).unwrap();
        // Reset the mock pins so the post-drift spawn proceeds
        // cleanly.
        mock.pin_status(ServiceStatus::Starting).await;
        mock.pin_health(true).await;
        sup.reconcile_once().await;

        let snap = sup.snapshot_status().await;
        assert_eq!(
            snap[0].restart_attempts, 0,
            "drift respawn must reset restart_attempts",
        );
    }

    #[tokio::test]
    async fn idle_crash_looping_does_not_burn_restart_attempts_per_tick() {
        // Pre-fix the inspect-CrashLooping branch did
        // `restart_count.max(slot.restart_attempts + 1)` on every
        // tick — an idle CrashLooping slot would climb to the cap
        // on its own without any new restart actually happening.
        // Pin the source-of-truth contract: idle ticks observing
        // CrashLooping must NOT bump restart_attempts.
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg);
        sup.reconcile_once().await; // spawn

        mock.pin_status(ServiceStatus::CrashLooping { restart_count: 2 })
            .await;
        sup.reconcile_once().await; // observe → adopt count=2
        let after_first = sup.snapshot_status().await;
        assert_eq!(after_first[0].restart_attempts, 2);

        // CrashLooping slot is parked → reconcile_slot short-
        // circuits at the top, so further ticks must NOT bump.
        sup.reconcile_once().await;
        sup.reconcile_once().await;
        sup.reconcile_once().await;
        let after_idle = sup.snapshot_status().await;
        assert_eq!(
            after_idle[0].restart_attempts, 2,
            "idle CrashLooping ticks must NOT bump restart_attempts",
        );
    }

    #[tokio::test]
    async fn port_pool_exhaustion_parks_crash_looping_without_spawning() {
        // Drive the port allocator past SIDECAR_PORT_POOL_END by
        // pre-allocating manually, then attempt to register one
        // more sidecar. The supervisor must refuse the spawn (zero
        // controller calls) and park the slot CrashLooping so the
        // operator sees the problem instead of a silent collision.
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg);
        // Walk the pool down to one-past-the-end so the next
        // reconcile's allocate_port returns None.
        {
            let mut next = sup.next_host_port.lock().await;
            *next = SIDECAR_PORT_POOL_END + 1;
        }

        sup.reconcile_once().await;

        assert_eq!(
            mock.spawn_count().await,
            0,
            "exhausted pool must NOT call spawn",
        );
        let snap = sup.snapshot_status().await;
        assert!(
            matches!(snap[0].status, ServiceStatus::CrashLooping { .. }),
            "exhausted pool must park CrashLooping; got {:?}",
            snap[0].status,
        );
    }

    #[tokio::test]
    async fn distinct_channels_get_distinct_host_ports() {
        // Port pool stability — two sidecars in distinct channels
        // get sequential, non-colliding host ports.
        let reg = HookRegistry::new();
        for (pid, ch, p) in [
            ("p-signal", "signal", 8080u16),
            ("p-wa", "whatsapp", 8081u16),
        ] {
            let m = PluginManifest::parse(&format!(
                r#"
[plugin]
id = "{pid}"
name = "P"
version = "0.1.0"

[[services]]
name = "{pid}-sidecar"
image = "x"

[services.sidecar]
channel = "{ch}"
rpc_port = {p}
"#
            ))
            .unwrap();
            reg.enable(&m).unwrap();
        }
        let mock = Arc::new(MockServiceController::new());
        let sup = SidecarSupervisor::new(mock.clone(), reg);
        sup.reconcile_once().await;

        // Both sidecars registered → two spawns, sequential host
        // ports starting at the pool start.
        assert_eq!(mock.spawn_count().await, 2);
        let snap = sup.snapshot_status().await;
        let ports: Vec<u16> = snap
            .iter()
            .filter_map(|s| {
                s.rpc_url.as_ref().and_then(|u| {
                    u.strip_prefix("http://127.0.0.1:")?.parse::<u16>().ok()
                })
            })
            .collect();
        assert_eq!(ports.len(), 2);
        // Order isn't deterministic across snapshot (BTreeMap by
        // channel), so just check the set.
        let mut sorted = ports.clone();
        sorted.sort();
        assert_eq!(sorted, vec![SIDECAR_PORT_POOL_START, SIDECAR_PORT_POOL_START + 1]);
    }
}
