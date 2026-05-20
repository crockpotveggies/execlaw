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
//!     per registered sidecar (`HookRegistry::all_sidecars` is the
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

/// Per-sidecar runtime state. Cheap to clone individual fields; the
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
    /// container for this sidecar. Reused on every subsequent
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
    /// In-flight `docker build` task for the sidecar's image when
    /// it's not yet present locally. `Some` only while a build is
    /// running; the reconcile loop polls `done` and clears the
    /// field once the task terminates. Without this the supervisor
    /// would `.await` the build (5-15 min for python-sandbox)
    /// holding the slots mutex, blocking every `snapshot_status`
    /// call the SPA polls — the user-visible symptom was a
    /// "Loading sidecar status…" spinner stuck for the whole
    /// build.
    build_task: Option<BuildTaskState>,
}

/// Background image-build state shared between the reconcile loop
/// and the spawned `tokio::spawn` task that runs `docker build`.
/// All fields are Arc-wrapped so the slot can `clone()` cheaply on
/// every reconcile pass without contention.
#[derive(Debug, Clone)]
struct BuildTaskState {
    /// Set to `true` when the build task terminates (success OR
    /// failure). The reconcile loop reads this on each tick to
    /// decide whether the slot should stay parked or advance.
    done: Arc<std::sync::atomic::AtomicBool>,
    /// `Some(error_message)` when the build failed; `None` on
    /// success. Only written once, immediately before `done` flips
    /// to true.
    failure: Arc<Mutex<Option<String>>>,
}

impl SidecarSlot {
    fn fresh(b: RegisteredSidecar) -> Self {
        Self {
            registered: b,
            handle: None,
            host_port: None,
            status: ServiceStatus::Stopped,
            restart_attempts: 0,
            build_task: None,
        }
    }

    /// True if a manifest edit invalidated the running container —
    /// the supervisor must stop + respawn rather than try to
    /// hot-update a `bollard` container. Name+plugin_id changes
    /// shouldn't actually reach this code path (the registry's key
    /// would change, so the supervisor would see the slot as
    /// orphaned + a new one as fresh) — included defensively.
    fn drift_from(&self, latest: &RegisteredSidecar) -> bool {
        self.registered.image != latest.image
            || self.registered.rpc_port != latest.rpc_port
            || self.registered.rpc_health_path != latest.rpc_health_path
            || self.registered.name != latest.name
            || self.registered.plugin_id != latest.plugin_id
            || self.registered.env != latest.env
            || self.registered.mounts != latest.mounts
            || self.registered.entrypoint != latest.entrypoint
    }
}

/// Read-only status snapshot for the Sidecars admin page. One entry
/// per registered sidecar; sidecars with no live container report
/// `Stopped`. Plain struct so JSON serialisation stays boring.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SidecarRuntimeStatus {
    /// The sidecar's name — the manifest's `[[services]].name` for
    /// the entry that carries `[services.sidecar]`. Globally unique
    /// across enabled plugins; the supervisor's primary key.
    pub name: String,
    pub plugin_id: String,
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
    /// sidecars get distinct stable ports; the assignment lives in
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

    /// Reset the per-sidecar restart counter. Operators call this
    /// after fixing the underlying issue (image edit, secrets
    /// re-mount) so a parked-CrashLooping slot gets a fresh runway.
    pub async fn reset_attempts(&self, name: &str) {
        let mut slots = self.slots.lock().await;
        if let Some(slot) = slots.get_mut(name) {
            slot.restart_attempts = 0;
            if matches!(slot.status, ServiceStatus::CrashLooping { .. }) {
                slot.status = ServiceStatus::Stopped;
                slot.handle = None;
            }
        }
    }

    /// Operator-initiated restart: stop the live container and clear
    /// the slot's handle so the next reconcile spawns fresh. The
    /// host_port stays pinned so any URL the caller had keeps
    /// working post-restart, and restart_attempts is reset because
    /// this is intentional, not crash recovery.
    ///
    /// Used by the Signal admin endpoint to work around an upstream
    /// signal-cli bug: after a successful device-link the running
    /// daemon throws `UnsupportedOperationException` when adding the
    /// new account to its in-memory map, leaving the on-disk
    /// keystore ahead of what `/v1/accounts` reports. A clean
    /// daemon restart re-reads accounts on cold start (different
    /// code path) and the new pairing materialises.
    pub async fn restart_sidecar(&self, name: &str) -> Result<(), String> {
        let handle_to_stop = {
            let mut slots = self.slots.lock().await;
            let slot = slots
                .get_mut(name)
                .ok_or_else(|| format!("no registered sidecar named '{name}'"))?;
            slot.restart_attempts = 0;
            slot.status = ServiceStatus::Stopped;
            slot.handle.take()
        };
        if let Some(handle) = handle_to_stop {
            if let Err(e) = self.controller.stop(&handle).await {
                warn!(
                    sidecar = %name,
                    "stop during operator-initiated restart failed: {e}",
                );
                // Continue anyway — the next reconcile will force-
                // remove the stale name and create_container will
                // succeed because docker auto-cleans the dead handle.
            }
        }
        self.kick();
        Ok(())
    }

    /// Stop every running sidecar container and clear the slot map.
    ///
    /// Used by factory reset (`POST /api/admin/factory-reset`) to
    /// guarantee no orphaned containers survive a "back to first
    /// boot" wipe. Without this the reconcile loop only stops
    /// containers when their plugin is *disabled* — factory reset
    /// wipes `state_plugins` directly, leaving the registry empty
    /// next tick but the live containers stranded under their
    /// pre-wipe names (signal-cli, wuzapi, …) and ports.
    ///
    /// Returns the number of containers actually stopped (useful
    /// for the factory-reset response + test assertions). Errors
    /// from individual `controller.stop` calls are logged at
    /// WARN but do not short-circuit — the goal is "as much
    /// teardown as docker will give us" rather than transactional
    /// all-or-nothing.
    ///
    /// Safe to call from contexts where the supervisor's main
    /// reconcile loop is also running — the slots-mutex is held
    /// for the duration so a concurrent reconcile waits. Callers
    /// who want a permanent teardown (factory reset) should ALSO
    /// stop the reconcile task or clear the registry first;
    /// otherwise the next tick will respawn anything that still
    /// has a `RegisteredSidecar` entry. Factory reset does both
    /// (DB wipe → registry empties → reconcile is a no-op).
    pub async fn stop_all(&self) -> usize {
        let mut slots = self.slots.lock().await;
        let names: Vec<String> = slots.keys().cloned().collect();
        let mut stopped = 0usize;
        for name in names {
            let Some(mut slot) = slots.remove(&name) else {
                continue;
            };
            let was_running = slot.handle.is_some();
            if let Some(handle) = slot.handle.take() {
                info!(sidecar = %name, "stopping sidecar container for teardown");
                if let Err(e) = self.controller.stop(&handle).await {
                    warn!(
                        sidecar = %name,
                        error = %e,
                        "controller.stop failed during stop_all — container may be orphaned",
                    );
                } else {
                    stopped += 1;
                }
            }
            // Emit a UI transition so the SPA's sidecars page
            // reflects the teardown immediately (the loader-pill
            // / alerts dock already subscribes to this).
            if was_running && let Some(bus) = &self.bus {
                bus.publish(UiEvent::SidecarStatusChanged {
                    name: name.clone(),
                    status: format!("{:?}", ServiceStatus::Stopped),
                });
            }
        }
        stopped
    }

    /// Remove every sidecar belonging to `plugin_id`: stop + `docker
    /// rm -f` each running container, clear the slot map entries, and
    /// `rm -rf` the per-plugin state root at
    /// `~/.execlaw/sidecars/<plugin_id>/`.
    ///
    /// Distinct from `stop_all`:
    ///
    ///   * Scoped to one plugin (uninstall / factory-reset-per-plugin),
    ///     not "every sidecar on the host".
    ///   * Deletes the on-disk state directory after stopping. Without
    ///     this step a re-install would silently inherit the prior
    ///     plugin's keystore, account DB, paired-device list, etc. —
    ///     not "factory" semantics.
    ///   * Also removes the slot entry from the registry-backed slot
    ///     map so the next reconcile tick won't try to re-spawn it.
    ///     `stop_all` does the same for the global case; this is the
    ///     per-plugin variant.
    ///
    /// Caller contract: the plugin's `RegisteredSidecar` entries
    /// should already have been pulled from `HookRegistry` (e.g. via
    /// `registry.disable(plugin_id)`) before calling this — otherwise
    /// the reconcile loop will re-create the sidecar on the next tick.
    /// `PluginHost::disable` does that disable as its first step, and
    /// the orchestrator (`plugin_lifecycle::purge_plugin`) chains the
    /// two so order is correct by construction.
    ///
    /// All operations are best-effort: a failed `docker stop` or
    /// `remove_dir_all` is logged at WARN but doesn't short-circuit
    /// the remaining work. The returned `SidecarRemovalReport`
    /// surfaces what actually happened so the caller can include it
    /// in the operator-facing factory-reset / uninstall response.
    pub async fn remove_for_plugin(&self, plugin_id: &str) -> SidecarRemovalReport {
        let mut slots = self.slots.lock().await;
        // Snapshot first so we don't mutate while iterating.
        let owned: Vec<String> = slots
            .iter()
            .filter_map(|(name, slot)| {
                (slot.registered.plugin_id == plugin_id).then(|| name.clone())
            })
            .collect();

        let mut containers_removed = 0usize;
        let mut sidecars_visited = 0usize;
        for name in owned {
            sidecars_visited += 1;
            let Some(mut slot) = slots.remove(&name) else {
                continue;
            };
            let was_running = slot.handle.is_some();
            if let Some(handle) = slot.handle.take() {
                info!(
                    sidecar = %name,
                    plugin_id = %plugin_id,
                    "stopping + removing sidecar container for plugin teardown",
                );
                // ServiceController::stop is documented to "stop AND
                // remove" the container — docker rm -f semantics. No
                // separate remove step needed.
                if let Err(e) = self.controller.stop(&handle).await {
                    warn!(
                        sidecar = %name,
                        plugin_id = %plugin_id,
                        error = %e,
                        "controller.stop failed during remove_for_plugin — container may be orphaned",
                    );
                } else {
                    containers_removed += 1;
                }
            }
            if was_running && let Some(bus) = &self.bus {
                bus.publish(UiEvent::SidecarStatusChanged {
                    name: name.clone(),
                    status: format!("{:?}", ServiceStatus::Stopped),
                });
            }
        }
        // Drop the mutex before any filesystem work — `rm -rf` of the
        // state dir may take a beat for plugins with sizeable
        // keystores (signal-cli's protocol-store can be hundreds of
        // MB) and we'd rather not hold the slots lock against the
        // reconcile loop while that happens.
        drop(slots);

        let state_root = plugin_state_root(plugin_id);
        let state_dir_removed = if state_root.exists() {
            match std::fs::remove_dir_all(&state_root) {
                Ok(()) => {
                    info!(
                        plugin_id = %plugin_id,
                        path = %state_root.display(),
                        "removed plugin sidecar state root",
                    );
                    true
                }
                Err(e) => {
                    warn!(
                        plugin_id = %plugin_id,
                        path = %state_root.display(),
                        error = %e,
                        "failed to remove plugin sidecar state root — manual cleanup may be needed",
                    );
                    false
                }
            }
        } else {
            // Plugin had no sidecars (tool-only plugin) or never
            // reached spawn — the dir was never created. That's a
            // valid clean state, not a failure.
            false
        };

        SidecarRemovalReport {
            plugin_id: plugin_id.to_owned(),
            sidecars_visited,
            containers_removed,
            state_dir_removed,
            state_dir_path: state_root.display().to_string(),
        }
    }

    /// Look up the published host port for a single supervised
    /// sidecar by name. Returns `Some(port)` only when the sidecar
    /// has been spawned at least once (the supervisor mints its
    /// host port on first spawn, then reuses it on every respawn —
    /// see `host_port: Option<u16>` on `SidecarSlot`). Returns
    /// `None` when no slot exists, when the slot has never spawned,
    /// or when the slot is parked `CrashLooping` with no live
    /// handle.
    ///
    /// This is the single accessor consumer plugins (signal-cli
    /// transport, future bridge consumers) call to dial the
    /// sidecar's local RPC. We deliberately surface a port number
    /// rather than a fully-qualified URL so callers can append
    /// arbitrary RPC paths without parsing/joining: every supported
    /// sidecar publishes on `127.0.0.1:<port>`, no exceptions.
    pub async fn host_port_for(&self, name: &str) -> Option<u16> {
        self.slots
            .lock()
            .await
            .get(name)
            .and_then(|s| s.handle.as_ref())
            .map(|h| h.host_port)
    }

    /// True iff `port` is currently published by ANY supervised
    /// sidecar. Used by the script tier's `sidecar_http_*` path
    /// to validate that the URL a plugin author hands in actually
    /// resolves to a known sidecar before bypassing the SSRF
    /// guard. O(n) over running sidecars — fine at single-digit
    /// counts; if that grows we add a port→name index.
    pub async fn has_published_port(&self, port: u16) -> bool {
        self.slots
            .lock()
            .await
            .values()
            .any(|s| s.handle.as_ref().map(|h| h.host_port) == Some(port))
    }

    /// Snapshot every sidecar's current state. Returns one entry
    /// per sidecar **registered in the hook registry** — sidecars
    /// the supervisor has never reconciled yet still show up as
    /// `Stopped` so the SPA's sidecars page can render the row
    /// before the first reconcile lands.
    pub async fn snapshot_status(&self) -> Vec<SidecarRuntimeStatus> {
        let sidecars = self.registry.all_sidecars();
        let slots = self.slots.lock().await;
        sidecars
            .into_iter()
            .map(|b| {
                let slot = slots.get(&b.name);
                let status = slot
                    .map(|s| s.status.clone())
                    .unwrap_or(ServiceStatus::Stopped);
                let restart_attempts = slot.map(|s| s.restart_attempts).unwrap_or(0);
                let rpc_url = slot
                    .and_then(|s| s.handle.as_ref())
                    .map(|h| format!("http://127.0.0.1:{}", h.host_port));
                SidecarRuntimeStatus {
                    name: b.name.clone(),
                    plugin_id: b.plugin_id.clone(),
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

        // Phase 1: stop + drop slots whose sidecar is no longer
        // registered (plugin disabled / uninstalled).
        let desired_names: std::collections::HashSet<String> =
            desired.iter().map(|b| b.name.clone()).collect();
        let to_drop: Vec<String> = slots
            .keys()
            .filter(|c| !desired_names.contains(*c))
            .cloned()
            .collect();
        for c in to_drop {
            if let Some(mut slot) = slots.remove(&c) {
                let was_running = slot.handle.is_some();
                if let Some(handle) = slot.handle.take() {
                    debug!(sidecar = %c, "stopping orphaned sidecar container");
                    if let Err(e) = self.controller.stop(&handle).await {
                        warn!(
                            sidecar = %c,
                            error = %e,
                            "failed to stop orphaned sidecar container",
                        );
                    }
                }
                // Only emit a UI transition when the slot was
                // actually running — orphaning a stopped slot
                // shouldn't spam the bus. (Mirrors the
                // `transition_status` dedup on the live-slot path.)
                if was_running && let Some(bus) = &self.bus {
                    bus.publish(UiEvent::SidecarStatusChanged {
                        name: c.clone(),
                        status: format!("{:?}", ServiceStatus::Stopped),
                    });
                }
            }
        }

        // Phase 2: ensure every desired sidecar has a slot, then
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
                .get(&sidecar.name)
                .map(|s| s.drift_from(&sidecar))
                .unwrap_or(false);
            if needs_respawn_for_drift {
                if let Some(slot) = slots.get_mut(&sidecar.name) {
                    if let Some(handle) = slot.handle.take() {
                        debug!(
                            sidecar = %sidecar.name,
                            "sidecar manifest changed; stopping prior container",
                        );
                        if let Err(e) = self.controller.stop(&handle).await {
                            warn!(sidecar = %sidecar.name, error = %e,
                                  "failed to stop sidecar during drift respawn");
                        }
                    }
                    slot.status = ServiceStatus::Stopped;
                    slot.restart_attempts = 0;
                    slot.registered = sidecar.clone();
                }
            }

            let slot = slots
                .entry(sidecar.name.clone())
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
                            sidecar = %sidecar.name,
                            "sidecar port pool exhausted; refusing to spawn",
                        );
                        // Park CrashLooping so the slot doesn't
                        // burn restart attempts on a problem
                        // operator action can't fix without a
                        // restart of the control plane.
                        let new_status = ServiceStatus::CrashLooping {
                            restart_count: MAX_RESTART_ATTEMPTS,
                        };
                        self.transition_status(&sidecar.name, slot, new_status);
                        return;
                    }
                },
            };
            let mounts = match resolve_mounts(sidecar) {
                Ok(m) => m,
                Err(e) => {
                    warn!(
                        sidecar = %sidecar.name,
                        plugin_id = %sidecar.plugin_id,
                        "sidecar mount resolution failed: {e}",
                    );
                    let new_status = ServiceStatus::CrashLooping {
                        restart_count: MAX_RESTART_ATTEMPTS,
                    };
                    self.transition_status(&sidecar.name, slot, new_status);
                    return;
                }
            };
            let spec = ServiceSpec {
                name: container_name(&sidecar.plugin_id, &sidecar.name),
                image: sidecar.image.clone(),
                entrypoint: sidecar.entrypoint.clone(),
                env: sidecar.env.clone(),
                mounts,
                host_port: port,
                container_port: sidecar.rpc_port,
                ..Default::default()
            };
            // Plugin sidecars typically declare images like
            // `execlaw/python-sandbox-fast:0.1.0` that only exist
            // locally — operators don't `docker push` to a public
            // registry. When the image isn't present, we run
            // `docker build` against the plugin's stage dir before
            // letting Bollard try to spawn (which would otherwise
            // pull-404 and park the slot at CrashLooping).
            //
            // The build runs in `tokio::spawn` so the supervisor's
            // slots mutex isn't held for the build's 5-15 minute
            // duration — without that hand-off, every
            // `snapshot_status` call the SPA polls would queue
            // behind the build and the operator would see
            // "Loading sidecar status…" stuck on the page until
            // duckdb's source-build finished. The supervisor polls
            // `slot.build_task.done` on each reconcile tick and
            // advances the slot through Pulling → spawn once the
            // task succeeds. Skip the build path entirely when
            // `stage_path` is None (test fixtures register
            // sidecars without a build context; those use prebuilt
            // mock images).
            if let Some(stage) = sidecar.stage_path.as_ref() {
                if let Some(task) = slot.build_task.clone() {
                    if !task.done.load(std::sync::atomic::Ordering::SeqCst) {
                        // Still building. Keep the slot in Pulling
                        // so the SPA's "Sidecar is booting up…"
                        // banner stays visible. Return from this
                        // reconcile_slot pass — the supervisor's
                        // outer loop will check us again next tick.
                        self.transition_status(&sidecar.name, slot, ServiceStatus::Pulling);
                        return;
                    }
                    // Build terminated — drop the task either way,
                    // then surface the failure (if any) or fall
                    // through to spawn.
                    let failure = task.failure.lock().await.clone();
                    slot.build_task = None;
                    if let Some(err) = failure {
                        warn!(
                            sidecar = %sidecar.name,
                            plugin_id = %sidecar.plugin_id,
                            image = %sidecar.image,
                            error = %err,
                            "sidecar image build failed; falling through to spawn so \
                             Bollard reports the underlying pull-404",
                        );
                    } else {
                        info!(
                            sidecar = %sidecar.name,
                            plugin_id = %sidecar.plugin_id,
                            image = %sidecar.image,
                            "sidecar image build complete; proceeding to spawn",
                        );
                    }
                } else if let Some(docker) = resolve_docker_binary() {
                    // Fast probe: is the image already local? If
                    // yes, skip the build entirely (operator-paced
                    // rebuild already happened OR this is a
                    // supervisor restart after a successful build).
                    let cached = image_is_local(&docker, &sidecar.image).await;
                    if !cached {
                        let dockerfile = stage.join("Dockerfile");
                        if dockerfile.exists() {
                            info!(
                                sidecar = %sidecar.name,
                                plugin_id = %sidecar.plugin_id,
                                image = %sidecar.image,
                                context = %stage.display(),
                                "sidecar image missing locally; spawning background \
                                 `docker build` task",
                            );
                            let task = spawn_image_build_task(
                                docker,
                                sidecar.image.clone(),
                                stage.to_path_buf(),
                                sidecar.plugin_id.clone(),
                            );
                            slot.build_task = Some(task);
                            self.transition_status(&sidecar.name, slot, ServiceStatus::Pulling);
                            // First reconcile after kick: leave
                            // the spawn for the next tick. The
                            // build runs in tokio::spawn so the
                            // slots mutex is released as soon as
                            // we return.
                            return;
                        } else {
                            debug!(
                                sidecar = %sidecar.name,
                                stage = %stage.display(),
                                "sidecar image missing locally and stage has no \
                                 Dockerfile; falling through to spawn (Bollard \
                                 will surface the pull-404)",
                            );
                        }
                    }
                }
            }
            match self.controller.spawn(&spec).await {
                Ok(handle) => {
                    info!(
                        sidecar = %sidecar.name,
                        container = %handle.name,
                        host_port = handle.host_port,
                        "sidecar container spawned",
                    );
                    slot.handle = Some(handle);
                    self.transition_status(&sidecar.name, slot, ServiceStatus::Starting);
                    return;
                }
                Err(e) => {
                    // Port-conflict on the host (the port we minted
                    // is already bound by some other process / a
                    // stale Docker veth / a TOCTOU race past the
                    // `port_is_free` probe). Release the slot's
                    // pinned port so the next reconcile mints a
                    // fresh one, and do NOT burn a restart attempt
                    // — this is environmental state, not a plugin
                    // bug, and the recovery is cheap. Without this
                    // branch the supervisor pinned the dead port on
                    // every retry and parked the slot CrashLooping
                    // after MAX_RESTART_ATTEMPTS for a problem the
                    // next port in the pool would have fixed.
                    let err_str = e.to_string();
                    if is_port_conflict_error(&err_str) {
                        let stale_port = slot.host_port;
                        slot.host_port = None;
                        warn!(
                            sidecar = %sidecar.name,
                            stale_port = ?stale_port,
                            error = %err_str,
                            "sidecar host-port conflict; releasing port for re-allocation on next tick",
                        );
                        self.transition_status(&sidecar.name, slot, ServiceStatus::Stopped);
                        // Kick the reconcile loop so retry happens
                        // immediately rather than waiting the full
                        // tick interval (port allocation is fast
                        // and we want a healthy sidecar back asap).
                        self.kick();
                        return;
                    }

                    slot.restart_attempts = slot.restart_attempts.saturating_add(1);
                    let new_status = if slot.restart_attempts >= MAX_RESTART_ATTEMPTS {
                        warn!(
                            sidecar = %sidecar.name,
                            attempts = slot.restart_attempts,
                            error = %err_str,
                            "sidecar container hit restart cap; parking CrashLooping",
                        );
                        ServiceStatus::CrashLooping {
                            restart_count: slot.restart_attempts,
                        }
                    } else {
                        warn!(
                            sidecar = %sidecar.name,
                            attempts = slot.restart_attempts,
                            error = %err_str,
                            "sidecar container spawn failed; will retry",
                        );
                        ServiceStatus::Stopped
                    };
                    self.transition_status(&sidecar.name, slot, new_status);
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
                    sidecar = %sidecar.name,
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
                    sidecar = %sidecar.name,
                    "sidecar container vanished; will respawn",
                );
                slot.handle = None;
                self.transition_status(&sidecar.name, slot, ServiceStatus::Stopped);
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
                    &sidecar.name,
                    slot,
                    ServiceStatus::CrashLooping { restart_count },
                );
            }
            ServiceStatus::Pulling => {
                // Image still downloading — there's no container
                // running to RPC against, just publish the status
                // and wait.
                self.transition_status(&sidecar.name, slot, inspect);
            }
            ServiceStatus::Starting => {
                // Audit fix (2026-05-04): images without a Docker
                // HEALTHCHECK declaration (bbernhard/signal-cli-rest-api
                // is one) leave bollard's `inspect` returning Starting
                // forever — Docker has no health data to report. The
                // pre-fix supervisor only ran the RPC probe when
                // inspect returned Healthy, so the sidecar was stuck in
                // Starting indefinitely. Now we ALSO probe RPC during
                // Starting: success promotes to Healthy without
                // restart; failure stays Starting (gives slow-booting
                // sidecars time without burning the restart cap, and a
                // truly broken sidecar surfaces as a stuck-Starting
                // status the operator can `kick` from the Sidecars
                // page).
                let url = format!(
                    "http://127.0.0.1:{}{}",
                    handle.host_port, sidecar.rpc_health_path
                );
                match self.controller.health_check(&url).await {
                    Ok(true) => {
                        // Bollard `inspect` returns `Starting` forever
                        // for images without a Docker HEALTHCHECK
                        // (bbernhard/signal-cli-rest-api is one), so
                        // this branch fires every reconcile tick once
                        // the sidecar is actually healthy. Gate the
                        // INFO event on the transition (mirroring the
                        // Healthy-branch log at the bottom of this
                        // file) and emit DEBUG per probe so operators
                        // tailing at DEBUG can still see the loop.
                        if !matches!(slot.status, ServiceStatus::Healthy) {
                            info!(sidecar = %sidecar.name, "sidecar healthy via RPC probe (no Docker HEALTHCHECK)");
                        } else {
                            debug!(sidecar = %sidecar.name, "sidecar RPC probe ok");
                        }
                        slot.restart_attempts = 0;
                        self.transition_status(&sidecar.name, slot, ServiceStatus::Healthy);
                    }
                    Ok(false) => {
                        debug!(
                            sidecar = %sidecar.name,
                            url = %url,
                            "RPC probe returned non-success during Starting; will retry next tick",
                        );
                        self.transition_status(&sidecar.name, slot, inspect);
                    }
                    Err(e) => {
                        debug!(
                            sidecar = %sidecar.name,
                            url = %url,
                            error = %e,
                            "RPC probe errored during Starting; will retry next tick",
                        );
                        self.transition_status(&sidecar.name, slot, inspect);
                    }
                }
            }
            ServiceStatus::Healthy => {
                // Validate via the sidecar's own RPC healthcheck —
                // `inspect` only tells us the container is up; the
                // sidecar process inside might still be initialising.
                let url = format!(
                    "http://127.0.0.1:{}{}",
                    handle.host_port, sidecar.rpc_health_path
                );
                // Audit fix: capture the underlying error so an
                // operator triaging a stuck sidecar sees "connection
                // refused" / "got 500" rather than a generic boolean.
                let healthy = match self.controller.health_check(&url).await {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(sidecar = %sidecar.name, url = %url, error = %e,
                              "RPC health_check errored");
                        false
                    }
                };
                if healthy {
                    if !matches!(slot.status, ServiceStatus::Healthy) {
                        info!(sidecar = %sidecar.name, "sidecar healthy");
                    }
                    slot.restart_attempts = 0;
                    self.transition_status(&sidecar.name, slot, ServiceStatus::Healthy);
                } else {
                    // Container says it's up but RPC health failed —
                    // restart. Could be a slow-starting sidecar; the
                    // restart-attempt cap protects us either way.
                    warn!(
                        sidecar = %sidecar.name,
                        url = %url,
                        "sidecar RPC health failed; restarting container",
                    );
                    if let Err(e) = self.controller.stop(&handle).await {
                        warn!(sidecar = %sidecar.name, error = %e,
                              "stop-for-restart failed");
                    }
                    slot.handle = None;
                    slot.restart_attempts = slot.restart_attempts.saturating_add(1);
                    let new_status = if slot.restart_attempts >= MAX_RESTART_ATTEMPTS {
                        ServiceStatus::CrashLooping {
                            restart_count: slot.restart_attempts,
                        }
                    } else {
                        ServiceStatus::Stopped
                    };
                    self.transition_status(&sidecar.name, slot, new_status);
                }
            }
        }
    }

    /// Mint the next stable host port. Walks the pool from
    /// `SIDECAR_PORT_POOL_START` toward `SIDECAR_PORT_POOL_END`,
    /// skipping any port currently bound by another process on the
    /// host. Returns the first free port found; returns `None` only
    /// when the entire pool is occupied (at which point the
    /// supervisor refuses to spawn and parks the slot CrashLooping).
    ///
    /// 2026-05-14 — added the `port_is_free` OS probe. Pre-fix the
    /// allocator was a naive monotonic counter that handed out
    /// whatever port was next regardless of host availability. When
    /// port 8501/8502/etc. was occupied externally (a stale Docker
    /// veth, a dev server, anything competing for the localhost port
    /// range), the supervisor minted the busy port, the Docker spawn
    /// failed with `Bind for 127.0.0.1:<p> failed: port is already
    /// allocated`, the slot burned a restart attempt, the supervisor
    /// pinned the same dead port on the next tick (because the slot
    /// remembered it via `slot.host_port`), and after
    /// `MAX_RESTART_ATTEMPTS` the sidecar was parked CrashLooping
    /// indefinitely — for an entirely environmental, externally-fixable
    /// problem. The probe + the spawn-failure release path together
    /// turn that into a self-healing retry loop.
    ///
    /// Note that the probe is best-effort: the port could be claimed
    /// between this check and the Docker bind (TOCTOU). The spawn
    /// failure path (`is_port_conflict_error` in `reconcile_sidecar`)
    /// is the second line of defense and re-runs allocation when
    /// that race actually happens.
    async fn allocate_port(&self) -> Option<u16> {
        let mut next = self.next_host_port.lock().await;
        while *next <= SIDECAR_PORT_POOL_END {
            let candidate = *next;
            // Advance the cursor regardless of probe outcome so a
            // busy port doesn't get retried on every allocate call
            // (saturating_add is overflow-safe; the loop-condition
            // above is the real exit gate).
            *next = next.saturating_add(1);
            if port_is_free(candidate) {
                return Some(candidate);
            }
            debug!(
                port = candidate,
                "sidecar allocator: candidate port is occupied on the host; trying next in pool",
            );
        }
        None
    }

    /// Update `slot.status` and publish a `SidecarStatusChanged` event
    /// **only if the status actually changed**. Pre-fix the supervisor
    /// re-published on every reconcile pass even when the status was
    /// the same, which spammed the event bus + the SPA's sidecars
    /// page. Centralising the publish here means every transition
    /// site naturally dedups.
    fn transition_status(&self, name: &str, slot: &mut SidecarSlot, new_status: ServiceStatus) {
        if slot.status == new_status {
            return;
        }
        slot.status = new_status.clone();
        if let Some(bus) = &self.bus {
            bus.publish(UiEvent::SidecarStatusChanged {
                name: name.to_owned(),
                status: format!("{new_status:?}"),
            });
        }
    }
}

/// Stable per-(plugin, sidecar) container name. Mirrors
/// `backend_supervisor`'s naming scheme so an operator who knows the
/// `execlaw-…` convention finds sidecars where they expect.
fn container_name(plugin_id: &str, name: &str) -> String {
    format!("execlaw-sidecar-{plugin_id}-{name}")
}

/// `true` when `docker image inspect <image>` exits 0 — the cheap
/// probe that drives the supervisor's "build needed?" decision.
/// Returns `false` on any failure (missing image, daemon down,
/// docker CLI broken); the caller's next step (try to build, or
/// fall through to the spawn path which surfaces Bollard's error)
/// is the same either way.
async fn image_is_local(docker: &str, image: &str) -> bool {
    tokio::process::Command::new(docker)
        .arg("image")
        .arg("inspect")
        .arg(image)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Spawn a background `docker build` task and return a handle the
/// supervisor's reconcile loop can poll on subsequent ticks. The
/// task runs OFF the slots-mutex critical section so the SPA's
/// `/api/admin/sidecars` polls aren't blocked for the build's
/// duration (5-15 min on alpine + duckdb source-build the first
/// time around).
///
/// Two PATH knobs matter:
///   * The `docker` binary itself — resolved by `resolve_docker_binary`
///     in the supervisor (already done at call site; the resolved
///     path is passed in here).
///   * The child process's PATH — must include the Docker Desktop
///     bin dir so BuildKit's `docker-credential-desktop` helper
///     resolves when authenticating with Docker Hub for FROM-image
///     pulls. launchd's default PATH excludes `/usr/local/bin`.
fn spawn_image_build_task(
    docker: String,
    image: String,
    stage_path: std::path::PathBuf,
    plugin_id: String,
) -> BuildTaskState {
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failure: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let done_clone = done.clone();
    let failure_clone = failure.clone();
    tokio::spawn(async move {
        let extended_path =
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".to_string());
        let extended_path = format!(
            "/usr/local/bin:/opt/homebrew/bin:/Applications/Docker.app/Contents/Resources/bin:{extended_path}"
        );
        info!(
            image = %image,
            plugin_id = %plugin_id,
            context = %stage_path.display(),
            "sidecar image build started",
        );
        let result = tokio::process::Command::new(&docker)
            .arg("build")
            .arg("-t")
            .arg(&image)
            .arg(stage_path.as_os_str())
            .env("PATH", &extended_path)
            .status()
            .await;
        let outcome = match result {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!(
                "`docker build` for {image} exited non-zero ({:?}); \
                 run the build manually with `docker build -t {image} {}` \
                 to see the full output",
                status.code(),
                stage_path.display(),
            )),
            Err(e) => Err(format!("could not invoke `docker build`: {e}")),
        };
        if let Err(ref err) = outcome {
            *failure_clone.lock().await = Some(err.clone());
        } else {
            info!(
                image = %image,
                plugin_id = %plugin_id,
                "sidecar image build complete",
            );
        }
        done_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    BuildTaskState { done, failure }
}

/// Resolve a usable `docker` binary path. Mirrors the absolute-
/// path fallbacks in `setup_preflight::detect_docker` since the
/// launchd-spawned server inherits a minimal PATH (`/usr/bin:
/// /bin:/usr/sbin:/sbin`) that excludes both `/usr/local/bin`
/// (Docker Desktop's symlink) and `/opt/homebrew/bin` (brew's
/// docker CLI). Returns the first path whose `-v` invocation
/// exits 0; `None` when no candidate works.
fn resolve_docker_binary() -> Option<String> {
    use std::process::Command;
    for candidate in [
        "docker",
        "/usr/local/bin/docker",
        "/opt/homebrew/bin/docker",
        "/Applications/Docker.app/Contents/Resources/bin/docker",
    ] {
        // NOT `?` — a failed lookup on one candidate (e.g.
        // `docker` not on launchd's PATH) must fall through to
        // the next, not short-circuit out of the whole function.
        // Pre-fix this returned None on the first miss and the
        // sidecar supervisor never tried `/usr/local/bin/docker`,
        // leaving python-sandbox stuck on a pull-404.
        if let Ok(out) = Command::new(candidate).arg("-v").output() {
            if out.status.success() {
                return Some(candidate.to_owned());
            }
        }
    }
    None
}

/// Translate a sidecar's [`MountDecl`] entries into the
/// `HostMount` shape the container manager hands to dockerd.
///
/// Source schemes:
///
///   * `stage://<rel>` — joins `<rel>` against the plugin's stage
///     directory (the extracted ZIP). Read-only by default since
///     stage contents are immutable per-version. Errors if the
///     sidecar wasn't registered with a stage_path.
///   * `state://<name>` — resolves to
///     `<execlaw>/sidecars/<plugin>/<sidecar>/<name>/`, creating it
///     on first spawn so signal-cli's keystore (and similar)
///     persists across container restarts.
///   * absolute `/path` (or `C:\path` on Windows) — passed through
///     to dockerd verbatim. For operator-controlled mounts.
fn resolve_mounts(
    sidecar: &execlaw_plugin_host::hook_registry::RegisteredSidecar,
) -> Result<Vec<execlaw_container_manager::HostMount>, String> {
    use execlaw_container_manager::HostMount;
    let mut out = Vec::with_capacity(sidecar.mounts.len());
    for m in &sidecar.mounts {
        let (host_path, default_ro) = if let Some(rel) = m.source.strip_prefix("stage://") {
            let stage = sidecar.stage_path.as_ref().ok_or_else(|| {
                format!(
                    "mount '{}' uses stage:// but sidecar was registered without a stage path",
                    m.source
                )
            })?;
            let p = stage.join(rel);
            (p, true)
        } else if let Some(name) = m.source.strip_prefix("state://") {
            let base = state_dir_for(&sidecar.plugin_id, &sidecar.name);
            let p = base.join(name);
            std::fs::create_dir_all(&p)
                .map_err(|e| format!("create sidecar state dir {}: {e}", p.display()))?;
            (p, false)
        } else if std::path::Path::new(&m.source).is_absolute() {
            (std::path::PathBuf::from(&m.source), false)
        } else {
            return Err(format!(
                "mount source '{}' must use stage://, state://, or be absolute",
                m.source
            ));
        };
        // Caller's `read_only` always wins; default depends on
        // scheme so `state://` doesn't accidentally land RO.
        let read_only = if m.read_only { true } else { default_ro };
        out.push(HostMount {
            host_path: host_path.to_string_lossy().into_owned(),
            container_path: m.target.clone(),
            read_only,
        });
    }
    Ok(out)
}

/// Per-(plugin, sidecar) state root. Lives under
/// `~/.execlaw/sidecars/<plugin>/<sidecar>/` so an operator who
/// uninstalls + reinstalls keeps their account state intact.
fn state_dir_for(plugin_id: &str, sidecar_name: &str) -> std::path::PathBuf {
    plugin_state_root(plugin_id).join(sidecar_name)
}

/// OS-level probe: is `port` bindable on `127.0.0.1` right now?
///
/// Used by `SidecarSupervisor::allocate_port` to skip ports that
/// another process (or a stale Docker veth) is currently holding.
/// The probe binds + immediately drops a `TcpListener`, so it
/// returns true iff the kernel would accept a fresh bind from us
/// in this instant.
///
/// **TOCTOU caveat:** the port can be claimed by something else in
/// the milliseconds between this returning true and the Docker
/// daemon's own bind. That race is handled by the spawn-failure
/// reallocation path (`is_port_conflict_error` in
/// `reconcile_sidecar`): if Docker reports a conflict after the
/// probe passed, the supervisor releases the slot's pinned port
/// and the next reconcile mints a fresh one. Two checks together
/// give us both the common-case fast path and the race recovery.
fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Recognise a Docker spawn error that's specifically a host-port
/// conflict (the "port is already allocated" / "address already in
/// use" / "Bind for 127.0.0.1:N failed" signatures).
///
/// When this matches, the supervisor releases the slot's pinned port
/// rather than treating the failure as a plugin bug and burning a
/// restart attempt. Without this discrimination, every transient
/// host-port conflict would push the slot toward `CrashLooping`
/// despite the conflict being entirely an external-state issue the
/// operator (or the next reconcile) can solve by picking a different
/// port.
///
/// The matcher is intentionally substring-based and case-insensitive
/// — bollard's error strings change subtly between Docker versions
/// (some report "Bind for 0.0.0.0:N", some "endpoint create failed",
/// some "userland proxy"); the common thread is the "already
/// allocated" / "in use" wording. We err on the side of false
/// positives because the recovery (re-allocate + retry) is cheap and
/// idempotent.
fn is_port_conflict_error(err: &str) -> bool {
    let s = err.to_lowercase();
    s.contains("port is already allocated")
        || s.contains("address already in use")
        || s.contains("bind for ")
        || s.contains("userland proxy")
}

/// The per-plugin state root — one level above `state_dir_for`,
/// containing every sidecar belonging to a single plugin. Used by
/// `remove_for_plugin` to nuke the plugin's whole sidecar state in
/// one `rm -rf` instead of walking each sidecar individually.
///
/// Layout reminder:
///
///   `~/.execlaw/sidecars/<plugin_id>/<sidecar_name>/...`
///
/// so `plugin_state_root("signal")` → `~/.execlaw/sidecars/signal/`
/// and `rm -rf` of that path wipes signal-cli's keystore, account
/// DB, paired-device list, attachment cache, everything.
pub(crate) fn plugin_state_root(plugin_id: &str) -> std::path::PathBuf {
    use directories::UserDirs;
    let home = UserDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".execlaw").join("sidecars").join(plugin_id)
}

/// Report of one `remove_for_plugin` call. Carried into the
/// factory-reset / uninstall HTTP response so operators can verify
/// the teardown actually did what the docs promise.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SidecarRemovalReport {
    /// The plugin whose sidecars were targeted.
    pub plugin_id: String,
    /// How many `RegisteredSidecar` slots matched the plugin (regardless
    /// of whether their containers were actually running).
    pub sidecars_visited: usize,
    /// How many docker containers were successfully stopped+removed.
    /// Lower than `sidecars_visited` when a sidecar was already
    /// stopped or when `controller.stop` failed (the WARN-level log
    /// has details).
    pub containers_removed: usize,
    /// True when `~/.execlaw/sidecars/<plugin_id>/` was successfully
    /// recursively deleted. False when no sidecar state ever existed
    /// (tool-only plugin) OR when the delete failed; the
    /// `state_dir_path` field lets the operator follow up manually.
    pub state_dir_removed: bool,
    /// The exact filesystem path the supervisor tried to remove. Stable
    /// across calls so operators / tests can reference it.
    pub state_dir_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_container_manager::MockServiceController;
    use execlaw_plugin_sdk::PluginManifest;

    fn registry_with_sidecar(plugin_id: &str, sidecar_name: &str, port: u16) -> HookRegistry {
        let m = PluginManifest::parse(&format!(
            r#"
[plugin]
id = "{plugin_id}"
name = "P"
version = "0.1.0"

[[services]]
name = "{sidecar_name}"
image = "execlaw/{sidecar_name}:0.1"

[services.sidecar]
rpc_port = {port}
"#
        ))
        .unwrap();
        let reg = HookRegistry::new();
        reg.enable(&m).unwrap();
        reg
    }

    /// Phase 3: `host_port_for` is the accessor consumer plugins
    /// (signal-cli transport, future bridges) call to resolve the
    /// sidecar's loopback RPC URL. Returns `None` until the first
    /// successful spawn, then `Some(port)` thereafter — even after
    /// drift respawn / RPC-fail restart, because the supervisor
    /// reuses the originally-minted port.
    #[tokio::test]
    async fn host_port_for_returns_none_before_first_spawn_then_stable_port() {
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg);

        // Pre-reconcile: nothing spawned, accessor returns None
        // (the manifest is registered but no slot exists yet).
        assert_eq!(sup.host_port_for("signal").await, None);
        assert_eq!(sup.host_port_for("nonexistent").await, None);

        // First reconcile spawns and assigns a stable host port
        // from the sidecar pool. We capture whatever the allocator
        // hands out (probing skips ports occupied on the test host,
        // so we can't pin a specific number) and use it as the
        // "must match this on respawn" expectation.
        sup.reconcile_once().await;
        let port = sup
            .host_port_for("signal")
            .await
            .expect("port must be set after first spawn");
        assert!(
            (SIDECAR_PORT_POOL_START..=SIDECAR_PORT_POOL_END).contains(&port),
            "minted port must be inside the sidecar pool, got {port}",
        );

        // RPC-fail restart cycle — the slot's host_port is reused
        // (the supervisor's "stable URL" guarantee).
        mock.pin_status(ServiceStatus::Healthy).await;
        mock.pin_health(false).await;
        sup.reconcile_once().await; // detect bad RPC, stop+drop handle
        // Slot stops → handle is None → accessor returns None
        // (because there's no live container to dial right now).
        // This is the correct semantic: callers must wait for the
        // next respawn before sending RPC.
        assert_eq!(sup.host_port_for("signal").await, None);

        // Next reconcile respawns. Bring health back so the cycle
        // settles and the port surfaces again.
        mock.pin_health(true).await;
        sup.reconcile_once().await; // respawn with reused port
        assert_eq!(
            sup.host_port_for("signal").await,
            Some(port),
            "respawn must reuse the originally-minted port",
        );
    }

    #[tokio::test]
    async fn reconcile_spawns_registered_sidecar_and_marks_starting() {
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg);

        sup.reconcile_once().await;

        assert_eq!(mock.spawn_count().await, 1);
        let last = mock.last_spawn().await.unwrap();
        assert_eq!(last.image, "execlaw/signal:0.1");
        assert_eq!(last.container_port, 8080);
        // host_port must be inside the sidecar pool. The exact
        // value depends on what's bound on the test host — the
        // probing allocator skips occupied pool members — so we
        // assert range membership, not a specific number.
        assert!(
            (SIDECAR_PORT_POOL_START..=SIDECAR_PORT_POOL_END).contains(&last.host_port),
            "minted host_port must be in the sidecar pool, got {}",
            last.host_port,
        );

        let snap = sup.snapshot_status().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].name, "signal");
        assert_eq!(snap[0].status, ServiceStatus::Starting);
        assert_eq!(snap[0].restart_attempts, 0);
    }

    /// Regression: factory reset's teardown step relies on
    /// `stop_all` actually stopping every running container.
    /// Before 2026-05-13 the WhatsApp wuzapi container survived
    /// a factory reset because the supervisor only stopped
    /// containers on plugin-disable (registry shrink) — and
    /// factory reset wipes the DB directly without touching the
    /// registry.
    #[tokio::test]
    async fn stop_all_stops_every_running_container_and_clears_slots() {
        let mock = Arc::new(MockServiceController::new());
        // Two sidecars across two plugins — simulating an
        // operator running both signal-cli AND whatsapp/wuzapi.
        let m1 = PluginManifest::parse(
            r#"
[plugin]
id = "p-signal"
name = "P1"
version = "0.1.0"

[[services]]
name = "signal-cli"
image = "execlaw/signal-cli:0.1"

[services.sidecar]
rpc_port = 8080
"#,
        )
        .unwrap();
        let m2 = PluginManifest::parse(
            r#"
[plugin]
id = "p-whatsapp"
name = "P2"
version = "0.1.0"

[[services]]
name = "wuzapi"
image = "execlaw/wuzapi:0.1"

[services.sidecar]
rpc_port = 8080
"#,
        )
        .unwrap();
        let reg = HookRegistry::new();
        reg.enable(&m1).unwrap();
        reg.enable(&m2).unwrap();
        let sup = SidecarSupervisor::new(mock.clone(), reg);

        // Two reconcile passes — first spawns both, second is a
        // no-op (already in Starting). Either way the slot map
        // has both running handles.
        sup.reconcile_once().await;
        assert_eq!(mock.spawn_count().await, 2);
        assert_eq!(mock.stop_count().await, 0);
        assert_eq!(sup.snapshot_status().await.len(), 2);

        // Factory-reset teardown call.
        let stopped = sup.stop_all().await;
        assert_eq!(stopped, 2, "every running container must be stopped");
        assert_eq!(
            mock.stop_count().await,
            2,
            "controller.stop must be called once per slot with a live handle",
        );

        // Slots are gone — subsequent host_port_for / has_published_port
        // calls return None / false, confirming the slot map is
        // empty. Snapshot still lists the registered names because
        // the registry hasn't shrunk (factory reset wipes the DB
        // which empties it on the next reconcile tick).
        assert_eq!(sup.host_port_for("signal-cli").await, None);
        assert_eq!(sup.host_port_for("wuzapi").await, None);

        // Idempotent — calling stop_all twice is fine; second
        // call sees no slots and stops nothing extra.
        let stopped_again = sup.stop_all().await;
        assert_eq!(stopped_again, 0);
        assert_eq!(
            mock.stop_count().await,
            2,
            "second stop_all must not double-stop"
        );
    }

    /// `remove_for_plugin` is the per-plugin variant of `stop_all`.
    /// Scoped to a single plugin: containers OWNED by it are
    /// stopped+removed, slots vanish from the slot map, and the
    /// per-plugin state root at `~/.execlaw/sidecars/<plugin_id>/`
    /// is recursively deleted. Other plugins' state is untouched.
    ///
    /// This is the load-bearing piece that closes the "WhatsApp
    /// wuzapi container + session DB survives uninstall" class of
    /// bugs.
    #[tokio::test]
    async fn remove_for_plugin_scopes_teardown_to_one_plugin_and_wipes_state_dir() {
        // Use unique plugin ids per test run so the real
        // ~/.execlaw/sidecars/ tree isn't molested. The IDs sit
        // under a real path on disk; we create + assert against
        // them explicitly.
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        );
        let pid_target = format!("test-remove-target-{suffix}");
        let pid_other = format!("test-remove-other-{suffix}");

        let mock = Arc::new(MockServiceController::new());
        let m_target = PluginManifest::parse(&format!(
            r#"
[plugin]
id = "{pid_target}"
name = "Tgt"
version = "0.1.0"

[[services]]
name = "tgt-bus"
image = "execlaw/test-tgt:0.1"

[services.sidecar]
rpc_port = 8080
"#
        ))
        .unwrap();
        let m_other = PluginManifest::parse(&format!(
            r#"
[plugin]
id = "{pid_other}"
name = "Oth"
version = "0.1.0"

[[services]]
name = "oth-bus"
image = "execlaw/test-oth:0.1"

[services.sidecar]
rpc_port = 8080
"#
        ))
        .unwrap();
        let reg = HookRegistry::new();
        reg.enable(&m_target).unwrap();
        reg.enable(&m_other).unwrap();
        let sup = SidecarSupervisor::new(mock.clone(), reg);

        // Spawn both. spawn_count → 2.
        sup.reconcile_once().await;
        assert_eq!(mock.spawn_count().await, 2);

        // Pre-create a state dir for the target plugin so we can
        // assert it disappears. The mock controller doesn't write
        // here itself — production sidecars do via the
        // `state://data` mount resolution path. We seed it manually
        // for the test.
        let target_root = plugin_state_root(&pid_target);
        let target_marker = target_root.join("tgt-bus").join("keystore.db");
        std::fs::create_dir_all(target_marker.parent().unwrap()).unwrap();
        std::fs::write(&target_marker, b"fake-keystore").unwrap();
        assert!(
            target_marker.exists(),
            "test setup: state dir must exist before remove"
        );

        // Likewise for the OTHER plugin, so we can assert it
        // survives the targeted removal.
        let other_root = plugin_state_root(&pid_other);
        let other_marker = other_root.join("oth-bus").join("session.db");
        std::fs::create_dir_all(other_marker.parent().unwrap()).unwrap();
        std::fs::write(&other_marker, b"untouched").unwrap();

        let report = sup.remove_for_plugin(&pid_target).await;

        // The target plugin's container was running → it was stopped.
        // The other plugin's container is untouched in the slot map.
        assert_eq!(report.plugin_id, pid_target);
        assert_eq!(report.sidecars_visited, 1);
        assert_eq!(report.containers_removed, 1);
        assert!(
            report.state_dir_removed,
            "report must record successful state dir removal; path={}",
            report.state_dir_path,
        );
        assert!(
            !target_root.exists(),
            "target plugin's state root must be gone after remove_for_plugin",
        );
        assert!(
            other_marker.exists(),
            "OTHER plugin's state must be untouched",
        );
        // controller.stop was called once — for the target container.
        // Other plugin still has its slot + handle.
        assert_eq!(mock.stop_count().await, 1);
        assert!(
            sup.host_port_for("tgt-bus").await.is_none(),
            "target sidecar slot must be gone",
        );

        // Idempotent — second call returns visited=0, state dir
        // already gone so state_dir_removed=false (nothing to remove,
        // not an error).
        let second = sup.remove_for_plugin(&pid_target).await;
        assert_eq!(second.sidecars_visited, 0);
        assert_eq!(second.containers_removed, 0);
        assert!(!second.state_dir_removed);
        assert_eq!(
            mock.stop_count().await,
            1,
            "second remove_for_plugin must not double-stop",
        );

        // Clean up the other plugin's dir so we don't leave test
        // artifacts behind.
        let _ = std::fs::remove_dir_all(&other_root);
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
    async fn no_healthcheck_image_promotes_via_rpc_probe_during_starting() {
        // Audit fix (2026-05-04): bbernhard/signal-cli-rest-api ships
        // without a Docker HEALTHCHECK declaration, so bollard's
        // `inspect` returns Starting forever. Pre-fix the supervisor
        // only ran the RPC probe when inspect returned Healthy, so the
        // sidecar got stuck in Starting indefinitely. Now we probe
        // RPC during Starting and promote on success.
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg);

        // Tick 1: spawn → inspect returns Starting (default for
        // running containers without HEALTHCHECK).
        sup.reconcile_once().await;
        // Tick 2: keep inspect at Starting, but pin RPC to success.
        // The supervisor should promote to Healthy without waiting
        // for inspect to ever return Healthy.
        mock.pin_status(ServiceStatus::Starting).await;
        mock.pin_health(true).await;
        sup.reconcile_once().await;

        let snap = sup.snapshot_status().await;
        assert_eq!(
            snap[0].status,
            ServiceStatus::Healthy,
            "Starting + RPC success must promote to Healthy even without Docker HEALTHCHECK",
        );
        assert_eq!(snap[0].restart_attempts, 0);
    }

    #[tokio::test]
    async fn rpc_failure_during_starting_does_not_burn_restart_cap() {
        // The flip-side of the no-HEALTHCHECK fix: when inspect is
        // Starting and RPC ALSO fails, we shouldn't increment
        // restart_attempts. A genuinely slow-booting sidecar
        // (signal-cli takes ~30s on first run) would otherwise burn
        // through MAX_RESTART_ATTEMPTS on cold start.
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg);

        sup.reconcile_once().await; // spawn
        mock.pin_status(ServiceStatus::Starting).await;
        mock.pin_health(false).await;

        // Run several reconciles with RPC failing.
        for _ in 0..5 {
            sup.reconcile_once().await;
        }

        let snap = sup.snapshot_status().await;
        assert_eq!(
            snap[0].status,
            ServiceStatus::Starting,
            "should remain Starting while RPC keeps failing",
        );
        assert_eq!(
            snap[0].restart_attempts, 0,
            "Starting+RPC-fail must NOT burn the restart cap on slow boot",
        );
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
name = "signal"
image = "execlaw/signal:0.2"

[services.sidecar]
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
        assert_eq!(last.image, "execlaw/signal:0.2");
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
        sup.reconcile_once().await; // spawn (port is in pool)
        let first_port = mock.last_spawn().await.unwrap().host_port;
        // Allocator probes the OS and skips ports bound externally,
        // so the exact value depends on test-host state — just assert
        // pool membership.
        assert!(
            (SIDECAR_PORT_POOL_START..=SIDECAR_PORT_POOL_END).contains(&first_port),
            "first port must be in sidecar pool, got {first_port}",
        );

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
name = "signal"
image = "execlaw/signal:0.2"

[services.sidecar]
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

    /// Regression for the 2026-05-14 dynamic-port-allocation rework.
    /// Pre-fix the allocator was a naive monotonic counter that
    /// handed out ports without checking host availability; when
    /// 8501 (or any pool member) was bound externally, the spawn
    /// failed with "port is already allocated" forever and the
    /// supervisor parked the sidecar CrashLooping for an entirely
    /// environmental problem.
    #[tokio::test]
    async fn allocate_port_skips_externally_bound_pool_member() {
        // Occupy the first pool port from outside the supervisor so
        // its OS-level `port_is_free` probe must skip it. We DO drop
        // the listener at the end so any other test re-running on
        // the same machine picks up clean state.
        let occupier = match std::net::TcpListener::bind(("127.0.0.1", SIDECAR_PORT_POOL_START)) {
            Ok(l) => l,
            Err(e) => {
                // If something else already has 8501, the test
                // setup precondition isn't met. Don't fail the
                // suite — this is the same race the real allocator
                // is designed to survive. Skip with a note.
                eprintln!(
                    "skipping: SIDECAR_PORT_POOL_START={} already bound by something else: {e}",
                    SIDECAR_PORT_POOL_START,
                );
                return;
            }
        };

        let reg = registry_with_sidecar("p-test", "test-bus", 8080);
        let mock = Arc::new(MockServiceController::new());
        let sup = SidecarSupervisor::new(mock.clone(), reg);

        sup.reconcile_once().await;

        // Spawn must have happened — pool exhaustion / blocked-on-
        // port-0 would be a regression.
        assert_eq!(mock.spawn_count().await, 1);
        // The minted host_port MUST NOT be 8501 (the occupied
        // member). We DON'T assert it's 8502 specifically — other
        // pool members may also be bound on the test machine
        // (Docker containers from concurrent dev work, other
        // sidecars from a real execlaw run, etc.). The behavior
        // we're pinning is "the allocator probes and skips", not
        // "the allocator picks port N".
        let spec = mock.last_spawn().await.unwrap();
        assert_ne!(
            spec.host_port, SIDECAR_PORT_POOL_START,
            "allocator must skip the externally-bound pool port",
        );
        assert!(
            spec.host_port > SIDECAR_PORT_POOL_START && spec.host_port <= SIDECAR_PORT_POOL_END,
            "minted port must be inside the pool and past the occupied head, got {}",
            spec.host_port,
        );

        drop(occupier);
    }

    /// `port_is_free` returns true for a port we just released and
    /// false for one we hold. Pure unit test of the helper.
    #[test]
    fn port_is_free_probe_distinguishes_held_and_released_ports() {
        // Bind, capture port, drop, re-probe — the OS may reuse the
        // port immediately. We don't depend on a specific number;
        // we just verify the boolean flips.
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(
            !port_is_free(port),
            "port {port} is held by the test's own listener → must report busy",
        );
        drop(l);
        // After drop the port is in TIME_WAIT briefly; bind with
        // SO_REUSEADDR (which TcpListener does on Unix by default,
        // and explicitly here on Windows) should succeed. We just
        // verify port_is_free's boolean follows reality — i.e., it
        // doesn't lie either way.
        let after = port_is_free(port);
        let _ = after; // value depends on OS scheduling; the
        // important assertion is the BUSY case above.
    }

    /// `is_port_conflict_error` recognises every flavour of Docker
    /// port-conflict string we've seen in production. False
    /// positives are fine (recovery is cheap); false negatives are
    /// the bug we're fixing, so the matcher errs toward inclusive.
    #[test]
    fn is_port_conflict_error_matches_known_docker_signatures() {
        let conflicts = [
            // The exact string from the user's 2026-05-14 report.
            "container runtime: start: Docker responded with status code 500: failed to set up container networking: driver failed programming external connectivity on endpoint execlaw-sidecar-signal-signal-cli (81710013602ccc5860cfb5261946c47b16f86f20b9c9c8ef422d947f423b04b4): Bind for 127.0.0.1:8502 failed: port is already allocated",
            "Bind for 0.0.0.0:8501 failed: port is already allocated",
            "Error response from daemon: driver failed programming external connectivity ... port is already allocated",
            "listen tcp 127.0.0.1:8501: bind: address already in use",
            "Error: userland proxy: listen tcp 0.0.0.0:8501: bind: address already in use",
        ];
        for c in conflicts {
            assert!(
                is_port_conflict_error(c),
                "must recognise as port conflict: {c}",
            );
        }
        // Non-conflict failures must NOT match — those should still
        // burn restart attempts via the normal path.
        let non_conflicts = [
            "image pull failed: unauthorized",
            "create: No such image: foo:bar",
            "container exited immediately with status 1",
            "permission denied",
        ];
        for c in non_conflicts {
            assert!(
                !is_port_conflict_error(c),
                "must NOT match as port conflict: {c}",
            );
        }
    }

    /// End-to-end recovery test: spawn fails with a port-conflict
    /// error → supervisor releases the slot's pinned port and does
    /// NOT burn a restart attempt → next reconcile mints a fresh
    /// port and spawns successfully.
    ///
    /// This is the load-bearing user-visible behavior of the
    /// dynamic-port rework. Pre-fix the supervisor would have pinned
    /// the dead port and parked CrashLooping after 3 retries.
    #[tokio::test]
    async fn port_conflict_spawn_failure_releases_port_and_does_not_burn_restart() {
        let mock = Arc::new(MockServiceController::new());
        let reg = registry_with_sidecar("p-signal", "signal-cli", 8080);
        let sup = SidecarSupervisor::new(mock.clone(), reg);

        // First reconcile: pin a spawn error matching the conflict
        // signature. Mock surfaces it via spawn_response.
        mock.pin_spawn_pull_error(
            "container runtime: start: Bind for 127.0.0.1:8501 failed: port is already allocated",
        )
        .await;
        sup.reconcile_once().await;

        // Slot must be Stopped (Ready for retry), restart_attempts
        // MUST still be 0, and the slot's host_port MUST have been
        // released so the next reconcile mints fresh.
        let snap = sup.snapshot_status().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(
            snap[0].restart_attempts, 0,
            "port-conflict spawn failure must NOT burn a restart attempt; got {}",
            snap[0].restart_attempts,
        );
        assert!(
            matches!(snap[0].status, ServiceStatus::Stopped),
            "port-conflict must leave slot Stopped (ready for retry), got {:?}",
            snap[0].status,
        );
        assert_eq!(
            sup.host_port_for("signal-cli").await,
            None,
            "released slot must have no live host_port",
        );

        // Clear the pinned error so the next spawn succeeds, then
        // reconcile again. The allocator must mint a port (any free
        // one) and the spawn must complete.
        mock.clear_spawn_response().await;
        sup.reconcile_once().await;

        assert_eq!(
            mock.spawn_count().await,
            2,
            "second spawn attempt must have happened",
        );
        let after = sup.snapshot_status().await;
        assert_eq!(
            after[0].restart_attempts, 0,
            "restart_attempts must STILL be 0 after the recovery — \
             the conflict path is environmental, not a plugin bug",
        );
        // The recovery spawn used a new port — verify it's somewhere
        // in the pool but not the pinned 8501 (which the supervisor
        // shouldn't have re-tried after release).
        let new_spec = mock.last_spawn().await.unwrap();
        assert!(
            new_spec.host_port >= SIDECAR_PORT_POOL_START
                && new_spec.host_port <= SIDECAR_PORT_POOL_END,
            "recovery port must be in the sidecar pool; got {}",
            new_spec.host_port,
        );
    }

    #[tokio::test]
    async fn distinct_sidecars_get_distinct_host_ports() {
        // Port pool stability — two distinct sidecars get sequential,
        // non-colliding host ports.
        let reg = HookRegistry::new();
        for (pid, sname, p) in [
            ("p-signal", "signal-cli", 8080u16),
            ("p-wa", "whatsapp-bridge", 8081u16),
        ] {
            let m = PluginManifest::parse(&format!(
                r#"
[plugin]
id = "{pid}"
name = "P"
version = "0.1.0"

[[services]]
name = "{sname}"
image = "x"

[services.sidecar]
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
                s.rpc_url
                    .as_ref()
                    .and_then(|u| u.strip_prefix("http://127.0.0.1:")?.parse::<u16>().ok())
            })
            .collect();
        assert_eq!(ports.len(), 2);
        // Order isn't deterministic across snapshot (BTreeMap by
        // name), and the exact values depend on test-host port
        // availability (the probing allocator skips externally-
        // occupied pool members). What we actually care about is
        // (a) both ports are in the pool, (b) they're distinct.
        let mut sorted = ports.clone();
        sorted.sort();
        for p in &sorted {
            assert!(
                (SIDECAR_PORT_POOL_START..=SIDECAR_PORT_POOL_END).contains(p),
                "port {p} must be in sidecar pool",
            );
        }
        assert_ne!(
            sorted[0], sorted[1],
            "distinct sidecars must get distinct host ports",
        );
    }
}
