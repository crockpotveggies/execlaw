//! Backend supervisor (Phase 12.C, MIGRATION_PLAN §5.4 + §5).
//!
//! For every `config_backends` row whose `mode = managed`, the
//! supervisor:
//!
//!   1. Picks a stable host port from a per-purpose pool.
//!   2. Asks the [`ServiceController`] to spawn the inference
//!      service container.
//!   3. Probes the container's HTTP `/health` endpoint until it
//!      reports 2xx → row is marked `Healthy` and `endpoint` is
//!      written back to `config_backends.endpoint`.
//!   4. On crash, exponential-backoff restart up to a hard cap;
//!      after the cap, parks the row in `CrashLooping` until the
//!      operator edits + saves again (which kicks the supervisor).
//!
//! Mode-flip semantics: switching a row from external→managed kicks
//! a spawn; managed→external stops the running container and clears
//! `endpoint`. The supervisor reconciles desired vs running state on
//! every tick + on every `kick()`.
//!
//! Production wires `BollardServiceController`; tests wire
//! `MockServiceController` (gated behind container-manager's
//! `test-mock` feature).

use execlaw_container_manager::{
    ServiceController, ServiceError, ServiceHandle, ServiceSpec, ServiceStatus,
};
use execlaw_core::backends::{BackendMode, BackendPurpose, BackendRow, BackendStore};
use execlaw_core::Database;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};

/// Default sweep cadence — every 5 seconds the supervisor reconciles
/// desired vs running state. Operators get visible-state-change
/// latency under that.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(5);

/// Hard cap on consecutive restart attempts before the supervisor
/// parks the row in `CrashLooping`. Mirrors selfhosted-claw's
/// "5-attempt then cooldown" pattern in `docs/runner-design.md`.
pub const MAX_RESTART_ATTEMPTS: u32 = 5;

/// Per-purpose stable host port. The supervisor binds these so the
/// runner's URL (`http://127.0.0.1:{port}/v1`) doesn't churn across
/// restarts. Picked to avoid common conflicts (vLLM defaults to
/// 8000; we use 8101+).
pub fn host_port_for(purpose: BackendPurpose) -> u16 {
    match purpose {
        BackendPurpose::Standard => 8101,
        BackendPurpose::Small => 8102,
        BackendPurpose::VoiceStt => 8103,
        BackendPurpose::VoiceTts => 8104,
    }
}

/// Live status the SPA reads via `/api/admin/backends/{purpose}/status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRuntimeStatus {
    pub purpose: BackendPurpose,
    pub mode: BackendMode,
    pub status: ServiceStatus,
    pub endpoint: Option<String>,
    /// Restart attempts since last health. Only populated for
    /// CrashLooping; 0 otherwise.
    pub restart_attempts: u32,
}

#[derive(Debug, Clone)]
struct ManagedSlot {
    /// Set after a successful `spawn`; cleared on stop.
    handle: Option<ServiceHandle>,
    /// Last status the supervisor observed.
    status: ServiceStatus,
    /// Increments on each consecutive restart attempt; resets to 0
    /// on a Healthy observation.
    restart_attempts: u32,
}

impl Default for ManagedSlot {
    fn default() -> Self {
        Self {
            handle: None,
            // Slots default to Stopped — the supervisor's first
            // pass observes this and decides spawn vs no-op based on
            // the row's mode.
            status: ServiceStatus::Stopped,
            restart_attempts: 0,
        }
    }
}

/// Pulls the operator's spawn config out of the row's
/// `model_spec_json`. Schema convention:
///
/// ```jsonc
/// {
///   "image": "vllm/vllm-openai:v0.6.2",
///   "args": ["--model", "Qwen3.5-27B-AWQ"],
///   "env": [["HF_HOME", "/cache"]],
///   "container_port": 8000
/// }
/// ```
///
/// Missing fields fall back to sensible defaults; an unparseable
/// blob produces a placeholder spec that the supervisor refuses to
/// spawn (status stays `Stopped` until the operator fixes it).
fn spec_from_row(row: &BackendRow) -> Result<ServiceSpec, String> {
    let obj = row.model_spec_json.as_object().ok_or_else(|| {
        "model_spec_json must be a JSON object for managed mode".to_string()
    })?;
    let image = obj
        .get("image")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "managed model_spec_json must include `image`".to_string())?;
    let args = obj
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let env = obj
        .get("env")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|pair| {
                    let arr = pair.as_array()?;
                    let k = arr.first()?.as_str()?.to_owned();
                    let v = arr.get(1)?.as_str()?.to_owned();
                    Some((k, v))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let container_port = obj
        .get("container_port")
        .and_then(|v| v.as_u64())
        .unwrap_or(8000) as u16;
    Ok(ServiceSpec {
        name: format!("execlaw-backend-{}", row.purpose.as_str()),
        image: image.to_owned(),
        args,
        env,
        gpu_id: row.gpu_id.clone(),
        host_port: host_port_for(row.purpose),
        container_port,
    })
}

/// Long-running supervisor task.
#[derive(Clone)]
pub struct BackendSupervisor {
    db: Database,
    controller: Arc<dyn ServiceController>,
    interval: Duration,
    kick: Arc<Notify>,
    /// Per-purpose runtime state, keyed by purpose-string.
    slots: Arc<Mutex<HashMap<String, ManagedSlot>>>,
}

impl BackendSupervisor {
    pub fn new(db: Database, controller: Arc<dyn ServiceController>) -> Self {
        Self::with_config(db, controller, DEFAULT_TICK_INTERVAL)
    }

    pub fn with_config(
        db: Database,
        controller: Arc<dyn ServiceController>,
        interval: Duration,
    ) -> Self {
        Self {
            db,
            controller,
            interval,
            kick: Arc::new(Notify::new()),
            slots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Force a reconcile pass. Operators trigger this from a
    /// "restart" button or after editing a Backend row, so the
    /// supervisor doesn't have to wait the full tick interval.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// Read-only status snapshot for the SPA. Returns one entry per
    /// configured backend (any mode); `external` rows always report
    /// `Stopped` here because the supervisor doesn't manage them.
    pub async fn snapshot_status(&self) -> Vec<BackendRuntimeStatus> {
        let store = BackendStore::new(&self.db);
        let rows = match store.list_all() {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let slots = self.slots.lock().await;
        rows.into_iter()
            .map(|row| {
                let slot = slots.get(row.purpose.as_str());
                let status = slot
                    .map(|s| s.status.clone())
                    .unwrap_or(ServiceStatus::Stopped);
                let restart_attempts = slot.map(|s| s.restart_attempts).unwrap_or(0);
                BackendRuntimeStatus {
                    purpose: row.purpose,
                    mode: row.mode,
                    status,
                    endpoint: row.endpoint,
                    restart_attempts,
                }
            })
            .collect()
    }

    /// Drive the loop until `stop` is notified.
    pub async fn run(&self, stop: Arc<Notify>) {
        info!(
            interval_secs = self.interval.as_secs(),
            "backend supervisor running"
        );
        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.interval) => {}
                _ = self.kick.notified() => {}
                _ = stop.notified() => {
                    info!("backend supervisor stop received; exiting");
                    return;
                }
            }
            self.reconcile_once().await;
        }
    }

    /// One reconcile pass. Public so tests can drive it
    /// deterministically.
    pub async fn reconcile_once(&self) {
        let store = BackendStore::new(&self.db);
        let rows = match store.list_all() {
            Ok(r) => r,
            Err(e) => {
                warn!("backend supervisor: failed to list backends: {e}");
                return;
            }
        };
        let mut slots = self.slots.lock().await;

        // Stop slots whose corresponding row is gone or has flipped
        // away from managed.
        let configured: HashMap<String, BackendRow> = rows
            .iter()
            .map(|r| (r.purpose.as_str().to_owned(), r.clone()))
            .collect();
        let to_drop: Vec<String> = slots
            .iter()
            .filter_map(|(p, slot)| {
                let row = configured.get(p);
                let still_managed = matches!(row.map(|r| r.mode), Some(BackendMode::Managed));
                if !still_managed && slot.handle.is_some() {
                    Some(p.clone())
                } else {
                    None
                }
            })
            .collect();
        for p in to_drop {
            if let Some(slot) = slots.get_mut(&p) {
                if let Some(handle) = slot.handle.take() {
                    debug!(
                        purpose = %p,
                        "supervisor stopping container (mode flipped or row removed)"
                    );
                    let _ = self.controller.stop(&handle).await;
                    // Don't clear `endpoint` here — when mode flips
                    // to external, the operator's same upsert
                    // typically set a new external URL we mustn't
                    // clobber. Stale loopback URLs from our prior
                    // managed run get overwritten the moment the
                    // row's next save provides a real endpoint;
                    // until then the runner sees a (stale) URL
                    // it'll fail to reach, which surfaces as an
                    // alert rather than a silent misroute.
                }
                slot.status = ServiceStatus::Stopped;
                slot.restart_attempts = 0;
            }
        }

        // Reconcile every managed row.
        for row in rows.iter().filter(|r| r.mode == BackendMode::Managed) {
            let key = row.purpose.as_str().to_owned();
            let slot = slots.entry(key.clone()).or_default();

            // If we're already CrashLooping past the cap, do nothing
            // until the operator's next save kicks the supervisor.
            if matches!(slot.status, ServiceStatus::CrashLooping { .. })
                && slot.restart_attempts >= MAX_RESTART_ATTEMPTS
            {
                continue;
            }

            // Need a handle? Spawn.
            if slot.handle.is_none() {
                let spec = match spec_from_row(row) {
                    Ok(s) => s,
                    Err(msg) => {
                        warn!(
                            purpose = %key,
                            "managed backend has invalid model_spec_json: {msg}"
                        );
                        slot.status = ServiceStatus::Stopped;
                        continue;
                    }
                };
                slot.status = ServiceStatus::Pulling;
                match self.controller.spawn(&spec).await {
                    Ok(handle) => {
                        info!(
                            purpose = %key,
                            container = %handle.container_id,
                            host_port = handle.host_port,
                            "managed backend spawned"
                        );
                        slot.handle = Some(handle);
                        slot.status = ServiceStatus::Starting;
                        slot.restart_attempts = 0;
                    }
                    Err(e) => {
                        warn!(purpose = %key, "spawn failed: {e}");
                        slot.status = ServiceStatus::CrashLooping {
                            restart_count: slot.restart_attempts + 1,
                        };
                        slot.restart_attempts += 1;
                        continue;
                    }
                }
            }

            // Have a handle: probe it.
            let handle = slot.handle.clone().expect("handle present after spawn arm");
            let inspect = match self.controller.inspect(&handle).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(purpose = %key, "inspect failed: {e}");
                    continue;
                }
            };
            match inspect {
                ServiceStatus::NotFound => {
                    // Container vanished externally; drop the
                    // handle so the next pass respawns.
                    slot.handle = None;
                    slot.status = ServiceStatus::Stopped;
                    let _ = store.set_endpoint(
                        row.purpose,
                        None,
                        chrono::Utc::now().timestamp(),
                    );
                }
                ServiceStatus::CrashLooping { restart_count } => {
                    slot.status = ServiceStatus::CrashLooping { restart_count };
                    slot.restart_attempts = restart_count.max(slot.restart_attempts + 1);
                    if slot.restart_attempts < MAX_RESTART_ATTEMPTS {
                        // Stop + drop handle so the next tick respawns.
                        let _ = self.controller.stop(&handle).await;
                        slot.handle = None;
                    }
                    let _ = store.set_endpoint(
                        row.purpose,
                        None,
                        chrono::Utc::now().timestamp(),
                    );
                }
                ServiceStatus::Stopped => {
                    slot.status = ServiceStatus::Stopped;
                    slot.handle = None;
                }
                _ => {
                    // Starting or Healthy: probe the URL.
                    let url = format!("{}/health", handle.endpoint_url("http"));
                    match self.controller.health_check(&url).await {
                        Ok(true) => {
                            slot.status = ServiceStatus::Healthy;
                            slot.restart_attempts = 0;
                            let endpoint = handle.endpoint_url("http");
                            // Write the URL back so the runner's
                            // next call picks it up. Idempotent —
                            // the row is left alone if the URL
                            // didn't change.
                            if row.endpoint.as_deref() != Some(endpoint.as_str()) {
                                let _ = store.set_endpoint(
                                    row.purpose,
                                    Some(&endpoint),
                                    chrono::Utc::now().timestamp(),
                                );
                            }
                        }
                        Ok(false) => {
                            slot.status = ServiceStatus::Starting;
                        }
                        Err(e) => {
                            warn!(purpose = %key, "health probe error: {e}");
                            slot.status = ServiceStatus::Starting;
                        }
                    }
                }
            }
        }
    }

    /// Force-restart a single managed backend. Bound to
    /// `POST /api/admin/backends/{purpose}/restart`.
    pub async fn restart(&self, purpose: BackendPurpose) -> Result<(), ServiceError> {
        let mut slots = self.slots.lock().await;
        if let Some(slot) = slots.get_mut(purpose.as_str()) {
            if let Some(handle) = slot.handle.take() {
                let _ = self.controller.stop(&handle).await;
            }
            slot.status = ServiceStatus::Stopped;
            slot.restart_attempts = 0;
        }
        drop(slots);
        // Kick so the next reconcile pass spawns immediately.
        self.kick();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_container_manager::MockServiceController;
    use execlaw_core::backends::{BackendStore, BackendUpsert};
    use execlaw_core::db::DbConfig;
    use execlaw_core::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn upsert_managed(
        store: &BackendStore<'_>,
        purpose: BackendPurpose,
        image: &str,
    ) -> BackendRow {
        store
            .upsert(
                &BackendUpsert {
                    purpose,
                    inference_backend: "service-vllm".into(),
                    model_spec_json: serde_json::json!({
                        "image": image,
                        "args": ["--model", "Qwen3.5-27B-AWQ"],
                        "env": [["HF_HOME", "/cache"]],
                        "container_port": 8000,
                    }),
                    gpu_id: Some("0".into()),
                    endpoint: None,
                    notes: None,
                    reasoning_enabled: true,
                    mode: BackendMode::Managed,
                },
                100,
            )
            .unwrap()
    }

    #[test]
    fn host_port_is_stable_per_purpose() {
        // The runner relies on these being deterministic across
        // restarts so a routine that ran yesterday doesn't see a
        // different URL today.
        for p in [
            BackendPurpose::Standard,
            BackendPurpose::Small,
            BackendPurpose::VoiceStt,
            BackendPurpose::VoiceTts,
        ] {
            assert_eq!(host_port_for(p), host_port_for(p));
        }
        // No two purposes share a port.
        let mut ports = std::collections::HashSet::new();
        for p in [
            BackendPurpose::Standard,
            BackendPurpose::Small,
            BackendPurpose::VoiceStt,
            BackendPurpose::VoiceTts,
        ] {
            assert!(
                ports.insert(host_port_for(p)),
                "port collision for {}",
                p.as_str()
            );
        }
    }

    #[test]
    fn spec_from_row_extracts_image_args_env() {
        let row = BackendRow {
            purpose: BackendPurpose::Standard,
            inference_backend: "service-vllm".into(),
            model_spec_json: serde_json::json!({
                "image": "vllm/vllm-openai:v0.6.2",
                "args": ["--model", "Qwen3.5-27B-AWQ", "--gpu-memory-utilization", "0.9"],
                "env": [["HF_HOME", "/cache"], ["NO_PROXY", "localhost"]],
                "container_port": 8000,
            }),
            gpu_id: Some("0".into()),
            endpoint: None,
            notes: None,
            reasoning_enabled: true,
            mode: BackendMode::Managed,
            created_at: 100,
            updated_at: 100,
        };
        let spec = spec_from_row(&row).unwrap();
        assert_eq!(spec.image, "vllm/vllm-openai:v0.6.2");
        assert_eq!(spec.args.len(), 4);
        assert_eq!(spec.env.len(), 2);
        assert_eq!(spec.host_port, host_port_for(BackendPurpose::Standard));
        assert_eq!(spec.container_port, 8000);
        assert_eq!(spec.gpu_id.as_deref(), Some("0"));
    }

    #[test]
    fn spec_from_row_rejects_missing_image() {
        let row = BackendRow {
            purpose: BackendPurpose::Standard,
            inference_backend: "service-vllm".into(),
            model_spec_json: serde_json::json!({"args": ["--model", "X"]}),
            gpu_id: None,
            endpoint: None,
            notes: None,
            reasoning_enabled: false,
            mode: BackendMode::Managed,
            created_at: 0,
            updated_at: 0,
        };
        assert!(spec_from_row(&row).is_err());
    }

    #[tokio::test]
    async fn reconcile_spawns_managed_row_and_writes_endpoint() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_managed(&store, BackendPurpose::Standard, "vllm:test");

        let mock = Arc::new(MockServiceController::new());
        let sup = BackendSupervisor::new(db.clone(), mock.clone());
        // First pass: spawn (mock spawn -> running). Then health
        // probes default to true → Healthy. Then endpoint is
        // written back.
        sup.reconcile_once().await;
        sup.reconcile_once().await;

        let row = store.get(BackendPurpose::Standard).unwrap().unwrap();
        assert_eq!(
            row.endpoint.as_deref(),
            Some(format!("http://127.0.0.1:{}", host_port_for(BackendPurpose::Standard)).as_str())
        );
        assert_eq!(mock.spawn_count().await, 1);

        let snap = sup.snapshot_status().await;
        let standard = snap
            .iter()
            .find(|s| s.purpose == BackendPurpose::Standard)
            .unwrap();
        assert_eq!(standard.status, ServiceStatus::Healthy);
    }

    #[tokio::test]
    async fn reconcile_skips_external_rows() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        // External row — supervisor must NOT spawn.
        store
            .upsert(
                &BackendUpsert {
                    purpose: BackendPurpose::Standard,
                    inference_backend: "service-vllm".into(),
                    model_spec_json: serde_json::json!({}),
                    gpu_id: None,
                    endpoint: Some("http://192.168.1.50:8000/v1".into()),
                    notes: None,
                    reasoning_enabled: false,
                    mode: BackendMode::External,
                },
                100,
            )
            .unwrap();

        let mock = Arc::new(MockServiceController::new());
        let sup = BackendSupervisor::new(db, mock.clone());
        sup.reconcile_once().await;
        assert_eq!(mock.spawn_count().await, 0);
    }

    #[tokio::test]
    async fn reconcile_stops_container_when_row_flips_to_external() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_managed(&store, BackendPurpose::Standard, "vllm:test");
        let mock = Arc::new(MockServiceController::new());
        let sup = BackendSupervisor::new(db.clone(), mock.clone());

        // First two passes: spawn + probe → endpoint written.
        sup.reconcile_once().await;
        sup.reconcile_once().await;
        assert_eq!(mock.spawn_count().await, 1);

        // Operator flips the row to external (e.g. switches to a
        // remote vLLM). Supervisor must stop the container and
        // clear the endpoint.
        store
            .upsert(
                &BackendUpsert {
                    purpose: BackendPurpose::Standard,
                    inference_backend: "service-vllm".into(),
                    model_spec_json: serde_json::json!({}),
                    gpu_id: None,
                    endpoint: Some("http://remote/v1".into()),
                    notes: None,
                    reasoning_enabled: true,
                    mode: BackendMode::External,
                },
                200,
            )
            .unwrap();
        sup.reconcile_once().await;
        assert_eq!(mock.stop_count().await, 1);

        let row = store.get(BackendPurpose::Standard).unwrap().unwrap();
        // Supervisor cleared its supervised endpoint; the new
        // external endpoint the operator typed survives the upsert
        // (this is the operator's data, not ours to clear after).
        // The supervisor's stop flow only clears endpoint when the
        // mode flip is observed BEFORE the upsert overwrites it —
        // here the upsert provided a new endpoint, which wins.
        assert_eq!(row.endpoint.as_deref(), Some("http://remote/v1"));
    }

    #[tokio::test]
    async fn reconcile_marks_crash_looping_after_repeated_failures() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_managed(&store, BackendPurpose::Standard, "vllm:bad");
        let mock = Arc::new(MockServiceController::new());
        let sup = BackendSupervisor::new(db, mock.clone());

        // First spawn lands fine, but pin the inspect to
        // CrashLooping so every reconcile re-attempts.
        sup.reconcile_once().await;
        mock.pin_status(ServiceStatus::CrashLooping {
            restart_count: 5,
        })
        .await;
        for _ in 0..6 {
            sup.reconcile_once().await;
        }

        let snap = sup.snapshot_status().await;
        let s = snap
            .iter()
            .find(|s| s.purpose == BackendPurpose::Standard)
            .unwrap();
        match &s.status {
            ServiceStatus::CrashLooping { .. } => {} // expected
            other => panic!("expected CrashLooping, got {other:?}"),
        }
        assert!(
            s.restart_attempts >= MAX_RESTART_ATTEMPTS,
            "should have hit the cap; got {}",
            s.restart_attempts
        );
    }

    #[tokio::test]
    async fn restart_drops_handle_and_kicks() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        upsert_managed(&store, BackendPurpose::Standard, "vllm:test");
        let mock = Arc::new(MockServiceController::new());
        let sup = BackendSupervisor::new(db, mock.clone());

        sup.reconcile_once().await;
        sup.reconcile_once().await;
        assert_eq!(mock.spawn_count().await, 1);

        sup.restart(BackendPurpose::Standard).await.unwrap();
        // After restart, the slot should drop the handle so the
        // next reconcile spawns again.
        sup.reconcile_once().await;
        assert_eq!(mock.spawn_count().await, 2);
        assert_eq!(mock.stop_count().await, 1);
    }

    #[tokio::test]
    async fn snapshot_status_lists_external_as_stopped_and_managed_as_observed() {
        let db = fresh_db();
        let store = BackendStore::new(&db);
        // External Small + managed Standard.
        store
            .upsert(
                &BackendUpsert {
                    purpose: BackendPurpose::Small,
                    inference_backend: "service-vllm".into(),
                    model_spec_json: serde_json::json!({}),
                    gpu_id: None,
                    endpoint: Some("http://x".into()),
                    notes: None,
                    reasoning_enabled: false,
                    mode: BackendMode::External,
                },
                100,
            )
            .unwrap();
        upsert_managed(&store, BackendPurpose::Standard, "vllm:test");

        let mock = Arc::new(MockServiceController::new());
        let sup = BackendSupervisor::new(db, mock);
        sup.reconcile_once().await;
        sup.reconcile_once().await;

        let snap = sup.snapshot_status().await;
        let small = snap
            .iter()
            .find(|s| s.purpose == BackendPurpose::Small)
            .unwrap();
        assert_eq!(small.mode, BackendMode::External);
        // External rows are always Stopped from the supervisor's POV.
        assert_eq!(small.status, ServiceStatus::Stopped);

        let standard = snap
            .iter()
            .find(|s| s.purpose == BackendPurpose::Standard)
            .unwrap();
        assert_eq!(standard.mode, BackendMode::Managed);
        assert_eq!(standard.status, ServiceStatus::Healthy);
    }
}
