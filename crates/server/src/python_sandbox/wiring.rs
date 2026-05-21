//! Boot-time wiring that turns the python-sandbox plugin's running
//! kernel-gateway sidecar into a live tool surface.
//!
//! Called once from `cli/main.rs` after `fire_on_enable_for_all`,
//! when the sidecar supervisor has had its first reconcile pass to
//! publish ports. If the plugin isn't installed, or its sidecar
//! isn't healthy yet, this is a no-op + a warning — the operator
//! can re-trigger by reinstalling the plugin (which restarts the
//! supervisor pass that will eventually call us again).
//!
//! Returns `Option<Arc<PythonSandboxService>>`:
//!   * `Some(svc)` — service constructed, tools registered. Caller
//!     keeps the Arc alive for the lifetime of the server so the
//!     output watcher's OS thread doesn't get dropped.
//!   * `None` — sidecar not ready (port not yet published). Tools
//!     are NOT registered; subsequent agent calls to `python.*`
//!     will hit "tool not found" until a restart.

use crate::events::EventBus;
use crate::python_sandbox::service::{
    PythonSandboxService, PythonSandboxSettings, ServiceError,
};
use crate::python_sandbox::tools::python_sandbox_tools;
use crate::sidecar_supervisor::SidecarSupervisor;
use execlaw_core::Database;
use execlaw_core::python_sandbox_config::PythonSandboxConfigStore;
use execlaw_plugin_host::HookRegistry;
use execlaw_plugin_host::hook_registry::RegisteredSidecar;
use execlaw_plugin_sdk::manifest::MountDecl;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// Operator-tunable bounds. Mirror the SPA Settings page's
/// IDLE_TIMEOUT_MIN / IDLE_TIMEOUT_MAX / MAX_OUTPUT_MIN /
/// MAX_OUTPUT_MAX constants so a value rejected by the form is
/// also rejected here as a second line of defense against a
/// hand-edited DB row.
const IDLE_TIMEOUT_MIN_SECS: u64 = 60;
const IDLE_TIMEOUT_MAX_SECS: u64 = 24 * 60 * 60;
const MAX_OUTPUT_MIN_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_MAX_BYTES: usize = 500 * 1024 * 1024;

/// Sidecar-naming constants — formerly read from the plugin
/// manifest; now hardcoded since python-sandbox is a native
/// feature. Container name stays `execlaw-sidecar-python-sandbox-
/// kernel-gateway` for backwards compatibility with operator
/// dev-installs that already have the image cached + a running
/// container the supervisor adopts on reconcile.
pub(crate) const SIDECAR_NAME: &str = "kernel-gateway";
pub(crate) const SIDECAR_IMAGE: &str = "execlaw/python-sandbox-fast:0.1.0";
pub(crate) const SIDECAR_RPC_PORT: u16 = 8888;
pub(crate) const SIDECAR_HEALTH_PATH: &str = "/api/kernels";

/// Synthetic "plugin id" used for sidecar bookkeeping (container
/// name composition, state-dir path, supervisor's per-source
/// keying). Preserves backwards compatibility with the previous
/// plugin install — the on-disk paths
/// (`~/.execlaw/sidecars/python-sandbox/kernel-gateway/`) and
/// container name don't churn for operators upgrading from the
/// plugin era.
pub(crate) const SIDECAR_SOURCE_ID: &str = "python-sandbox";

#[derive(Debug, Error)]
pub enum WireError {
    #[error("python_sandbox service setup failed: {0}")]
    Service(#[from] ServiceError),
    #[error("tool registration failed: {0}")]
    Register(String),
    #[error("native sidecar registration failed: {0}")]
    SidecarRegister(String),
    #[error("config read failed: {0}")]
    Config(String),
}

/// 2026-05-20 — register the kernel-gateway sidecar with the
/// HookRegistry as a NATIVE sidecar (not via plugin discovery).
/// Must run BEFORE the SidecarSupervisor's first reconcile pass
/// so the container spawns automatically.
///
/// Returns `Ok(false)` (didn't register) when:
///   * python-sandbox is disabled in `config_python_sandbox`
///     (operator hasn't opted in, or boot just disabled because
///     Docker is unreachable)
///   * docker_available is `false` — the sidecar can't possibly
///     spawn without Docker, and additionally this triggers the
///     auto-disable side effect so the next boot doesn't re-try.
///
/// Idempotent across separate processes (state lives in DB), but
/// NOT idempotent within one process: calling twice will fail at
/// `register_native_sidecar` with "already registered". Call
/// exactly once at boot, before reconcile.
pub fn register_native_sidecar_if_enabled(
    registry: &HookRegistry,
    db: &Database,
    docker_available: bool,
    now_unix: i64,
) -> Result<bool, WireError> {
    let store = PythonSandboxConfigStore::new(db);
    let cfg = store.get().map_err(|e| WireError::Config(e.to_string()))?;
    if !cfg.enabled {
        tracing::debug!(
            target: "python_sandbox::wiring",
            "python_sandbox disabled; skipping sidecar registration"
        );
        return Ok(false);
    }
    if !docker_available {
        // Auto-disable so the next boot doesn't keep WARNing about
        // an impossible sidecar. The Settings page surfaces this
        // (config reflects the auto-disable) so the operator sees
        // why the toggle flipped off.
        tracing::info!(
            "python_sandbox: Docker not detected; auto-disabling. \
             Install Docker Desktop and re-enable from Settings → Python sandbox."
        );
        if let Err(e) = store.disable(now_unix) {
            tracing::warn!(error = %e, "python_sandbox: auto-disable write failed");
        }
        return Ok(false);
    }

    registry
        .register_native_sidecar(python_sandbox_sidecar_spec())
        .map_err(WireError::SidecarRegister)?;
    tracing::info!(
        target: "python_sandbox::wiring",
        image = SIDECAR_IMAGE,
        rpc_port = SIDECAR_RPC_PORT,
        "python_sandbox: native sidecar registered; supervisor will spawn on reconcile"
    );
    Ok(true)
}

/// Canonical sidecar spec for the python-sandbox kernel-gateway.
/// Mirror of what the plugin manifest used to declare; now
/// hardcoded since the feature is native. Mounts use
/// `state://work` (special source the supervisor resolves to the
/// per-feature state dir) so the bind-mount path is identical to
/// the legacy plugin install.
pub(crate) fn python_sandbox_sidecar_spec() -> RegisteredSidecar {
    RegisteredSidecar {
        plugin_id: SIDECAR_SOURCE_ID.to_owned(),
        name: SIDECAR_NAME.to_owned(),
        image: SIDECAR_IMAGE.to_owned(),
        rpc_port: SIDECAR_RPC_PORT,
        rpc_health_path: SIDECAR_HEALTH_PATH.to_owned(),
        env: Vec::new(),
        mounts: vec![MountDecl {
            source: "state://work".to_owned(),
            target: "/work".to_owned(),
            read_only: false,
        }],
        entrypoint: None,
        stage_path: None,
    }
}

/// Live-toggle register. Used by the admin PUT handler when the
/// operator flips the toggle on AFTER boot. Idempotent — calling
/// twice (or after the boot path already registered) is a no-op
/// success. Returns `Ok(true)` if a fresh slot was inserted,
/// `Ok(false)` if the slot already existed with the same spec.
pub fn register_now(registry: &HookRegistry) -> Result<bool, WireError> {
    let spec = python_sandbox_sidecar_spec();
    // Capture whether a slot already existed so we can report
    // back. `register_native_sidecar_idempotent` swallows
    // identical re-registers; we infer by querying after.
    let pre_existed = registry.sidecar(SIDECAR_NAME).is_some();
    registry
        .register_native_sidecar_idempotent(spec)
        .map_err(WireError::SidecarRegister)?;
    if !pre_existed {
        tracing::info!(
            target: "python_sandbox::wiring",
            image = SIDECAR_IMAGE,
            rpc_port = SIDECAR_RPC_PORT,
            "python_sandbox: live-registered native sidecar (toggle-on); supervisor will spawn on reconcile"
        );
    }
    Ok(!pre_existed)
}

/// Live-toggle unregister. Used by the admin PUT handler when the
/// operator flips the toggle off. Drops the sidecar slot AND the
/// python.* builtins so the agent can't call them anymore. The
/// supervisor's next reconcile tick observes the missing slot and
/// stops the container.
///
/// Returns `(sidecar_dropped, tool_count_dropped)` for the log
/// breadcrumb / response envelope.
pub fn unregister_now(registry: &HookRegistry) -> (bool, usize) {
    let dropped = registry.unregister_native_sidecar(SIDECAR_NAME);
    let tool_count = registry.unregister_builtins_by_prefix("python.");
    if dropped || tool_count > 0 {
        tracing::info!(
            target: "python_sandbox::wiring",
            sidecar_dropped = dropped,
            tools_dropped = tool_count,
            "python_sandbox: live-unregistered (toggle-off); supervisor will stop the container on next reconcile"
        );
    }
    (dropped, tool_count)
}

/// Wire the python-sandbox tool surface into the running host.
/// Must run AFTER the supervisor has had a chance to publish the
/// sidecar's host port (typically ~2s after registration).
/// Idempotent within one process: a second call will fail at
/// `register_builtins` with "already registered".
///
/// 2026-05-20 — gating contract (post plugin → native migration):
///
///   * **Feature disabled in `config_python_sandbox.enabled`** →
///     silent `Ok(None)` at DEBUG. No mention in operator-visible
///     logs. Fresh-install default; also the auto-disabled state
///     when Docker is missing.
///
///   * **Feature enabled, sidecar not yet healthy** (port not
///     published — first reconcile in flight, or container
///     crash-looping) → WARN. Operator opted in; surfacing the
///     sidecar problem is the right UX.
///
///   * **Feature enabled + sidecar healthy** → wire normally,
///     INFO with the gateway URL + tool count.
pub async fn wire_python_sandbox(
    supervisor: &SidecarSupervisor,
    registry: &HookRegistry,
    db: &Database,
    events: &EventBus,
    now_unix: i64,
) -> Result<Option<Arc<PythonSandboxService>>, WireError> {
    // --- Gate #1: is the feature enabled at all? --------------
    let cfg = PythonSandboxConfigStore::new(db)
        .get()
        .map_err(|e| WireError::Config(e.to_string()))?;
    if !cfg.enabled {
        tracing::debug!(
            target: "python_sandbox::wiring",
            "python_sandbox disabled; skipping tool wiring silently"
        );
        return Ok(None);
    }

    // --- Gate #2: feature enabled, sidecar up? ----------------
    let Some(port) = supervisor.host_port_for(SIDECAR_NAME).await else {
        tracing::warn!(
            "python_sandbox: kernel-gateway sidecar not ready; python.* tools \
             will be unavailable this boot. Check supervisor logs."
        );
        return Ok(None);
    };

    let gateway_url = format!("http://127.0.0.1:{port}");
    tracing::info!(%gateway_url, "python_sandbox: wiring service");

    // State root — matches the supervisor's bind-mount source for
    // `state://work`. Same on-disk path as the legacy plugin
    // install (the supervisor's `plugin_state_root` helper keys
    // on the synthetic `SIDECAR_SOURCE_ID`).
    let sidecar_state =
        crate::sidecar_supervisor::plugin_state_root(SIDECAR_SOURCE_ID).join(SIDECAR_NAME);
    let work_root = sidecar_state.join("work");
    let artifacts_root = crate::host_caps_impl::builtin_artifacts_root_path();

    // Read operator-configured tunables from the native config
    // store (formerly read from the plugin-settings vault). Out-
    // of-range values fall back to module defaults rather than
    // fail wiring — settings shouldn't be a boot-blocker.
    let settings = PythonSandboxSettings {
        idle_timeout: clamp_idle_timeout(cfg.idle_timeout_seconds),
        max_output_bytes: clamp_max_output(cfg.max_output_bytes),
    };
    let service = PythonSandboxService::new_with_settings(
        gateway_url,
        work_root,
        artifacts_root,
        db.clone(),
        events.clone(),
        settings,
    )?;

    let tools = python_sandbox_tools(service.clone());
    let count = tools.len();
    execlaw_plugin_host::register_builtins(registry, db, now_unix, tools)
        .map_err(|e| WireError::Register(format!("{e:?}")))?;

    tracing::info!(
        count,
        "python_sandbox: registered python.* tool surface against live kernel-gateway"
    );
    Ok(Some(service))
}

/// Clamp idle-timeout to the allowed range; out-of-range falls
/// back to "use default" (None). Logged at warn for operator
/// visibility into hand-edited DB rows.
fn clamp_idle_timeout(secs: u32) -> Option<Duration> {
    let s = secs as u64;
    if s == 0 {
        return None;
    }
    if s < IDLE_TIMEOUT_MIN_SECS || s > IDLE_TIMEOUT_MAX_SECS {
        tracing::warn!(
            value = s,
            min_secs = IDLE_TIMEOUT_MIN_SECS,
            max_secs = IDLE_TIMEOUT_MAX_SECS,
            "python_sandbox: idle_timeout_seconds out of range; using default"
        );
        return None;
    }
    Some(Duration::from_secs(s))
}

fn clamp_max_output(bytes: u64) -> Option<usize> {
    if bytes == 0 {
        return None;
    }
    let n = bytes as usize;
    if n < MAX_OUTPUT_MIN_BYTES || n > MAX_OUTPUT_MAX_BYTES {
        tracing::warn!(
            value = n,
            min = MAX_OUTPUT_MIN_BYTES,
            max = MAX_OUTPUT_MAX_BYTES,
            "python_sandbox: max_output_bytes out of range; using default"
        );
        return None;
    }
    Some(n)
}

// ===================================================================
// Tests — offline checks only. Real boot is in cli/main.rs; the
// "service constructs against a live port" path is covered by the
// live_execute_tool_round_trip integration test in tools.rs.
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::{Database, DbConfig};
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_core::python_sandbox_config::{
        PythonSandboxConfigStore, PythonSandboxConfigUpdate,
    };

    fn open_test_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn enable_in_config(db: &Database) {
        PythonSandboxConfigStore::new(db)
            .update(
                PythonSandboxConfigUpdate {
                    enabled: Some(true),
                    ..Default::default()
                },
                0,
            )
            .unwrap();
    }

    // ----------------------------------------------------------
    // Native-config encapsulation contract (2026-05-20). Mirrors
    // the operator-facing rule: the host crate must not log
    // anything python-sandbox-specific on a fresh boot with the
    // feature off (default state).
    // ----------------------------------------------------------

    #[test]
    fn fresh_install_is_disabled_by_default() {
        // Migration 0011 seeds enabled=0. Nothing else is needed
        // for the host to skip wiring silently.
        let db = open_test_db();
        let cfg = PythonSandboxConfigStore::new(&db).get().unwrap();
        assert!(!cfg.enabled);
    }

    #[test]
    fn register_native_sidecar_skips_when_disabled() {
        let db = open_test_db();
        let registry = HookRegistry::new();
        let registered =
            register_native_sidecar_if_enabled(&registry, &db, /* docker */ true, 0).unwrap();
        assert!(
            !registered,
            "disabled feature must NOT register a sidecar — \
             this is the encapsulation rule applied to native config"
        );
    }

    #[test]
    fn register_native_sidecar_skips_when_no_docker() {
        // Enabled in config but Docker unreachable → register
        // returns false AND auto-disables in config so the next
        // boot doesn't re-try.
        let db = open_test_db();
        enable_in_config(&db);
        let registry = HookRegistry::new();
        let registered =
            register_native_sidecar_if_enabled(&registry, &db, /* docker */ false, 100).unwrap();
        assert!(!registered, "no Docker → no sidecar registration");
        let cfg = PythonSandboxConfigStore::new(&db).get().unwrap();
        assert!(
            !cfg.enabled,
            "no-Docker boot must auto-disable so the next boot \
             stays silent (Apple Silicon path)"
        );
    }

    #[test]
    fn register_native_sidecar_works_when_enabled_and_docker_available() {
        let db = open_test_db();
        enable_in_config(&db);
        let registry = HookRegistry::new();
        let registered =
            register_native_sidecar_if_enabled(&registry, &db, /* docker */ true, 0).unwrap();
        assert!(registered, "enabled + docker → register");
    }

    // ----------------------------------------------------------
    // Tunable clamping — operator-supplied values that fall
    // outside the bounds get rejected (back to default) rather
    // than block boot or be silently honored.
    // ----------------------------------------------------------

    #[test]
    fn clamp_idle_timeout_accepts_in_range() {
        assert_eq!(
            clamp_idle_timeout(900),
            Some(Duration::from_secs(900)),
            "default 900s in range"
        );
        assert_eq!(clamp_idle_timeout(60), Some(Duration::from_secs(60)), "min");
        assert_eq!(
            clamp_idle_timeout(86400),
            Some(Duration::from_secs(86400)),
            "max"
        );
    }

    #[test]
    fn clamp_idle_timeout_rejects_out_of_range() {
        assert!(clamp_idle_timeout(0).is_none(), "0 falls back to default");
        assert!(clamp_idle_timeout(30).is_none(), "below 60 → reject");
        assert!(clamp_idle_timeout(90000).is_none(), "above 24h → reject");
    }

    #[test]
    fn clamp_max_output_accepts_in_range() {
        assert_eq!(
            clamp_max_output(50 * 1024 * 1024),
            Some(50 * 1024 * 1024)
        );
        assert_eq!(clamp_max_output(1024 * 1024), Some(1024 * 1024), "min");
        assert_eq!(
            clamp_max_output(500 * 1024 * 1024),
            Some(500 * 1024 * 1024),
            "max"
        );
    }

    #[test]
    fn clamp_max_output_rejects_out_of_range() {
        assert!(clamp_max_output(0).is_none());
        assert!(clamp_max_output(1024).is_none(), "below 1 MiB → reject");
        assert!(
            clamp_max_output(600 * 1024 * 1024).is_none(),
            "above 500 MiB → reject"
        );
    }

    #[test]
    fn sidecar_constants_are_internally_consistent() {
        // The supervisor's container-naming uses
        // `execlaw-sidecar-<SIDECAR_SOURCE_ID>-<SIDECAR_NAME>`.
        // Pin the constants so an accidental rename of either
        // breaks tests FIRST instead of churning operator
        // dev-installs that have the existing container adopted.
        assert_eq!(SIDECAR_NAME, "kernel-gateway");
        assert_eq!(SIDECAR_SOURCE_ID, "python-sandbox");
        assert_eq!(SIDECAR_RPC_PORT, 8888);
        assert_eq!(SIDECAR_HEALTH_PATH, "/api/kernels");
    }
}
