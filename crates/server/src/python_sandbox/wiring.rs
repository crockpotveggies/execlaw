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

use crate::python_sandbox::service::{PythonSandboxService, ServiceError, PLUGIN_ID};
use crate::python_sandbox::tools::python_sandbox_tools;
use crate::sidecar_supervisor::SidecarSupervisor;
use execlaw_core::Database;
use execlaw_plugin_host::HookRegistry;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

/// Service name the manifest declares for the kernel-gateway sidecar.
/// The supervisor's `host_port_for` lookup keys on this; if you
/// rename the `[[services]]` entry in `plugins/python-sandbox/plugin.toml`,
/// bump this too.
const SIDECAR_NAME: &str = "kernel-gateway";

#[derive(Debug, Error)]
pub enum WireError {
    #[error("python_sandbox service setup failed: {0}")]
    Service(#[from] ServiceError),
    #[error("tool registration failed: {0}")]
    Register(String),
}

/// Wire the python-sandbox plugin's tool surface into the running
/// host. Idempotent within one process: a second call will fail at
/// register_builtins with a "already registered" conflict — call
/// this exactly once at boot.
pub async fn wire_python_sandbox(
    supervisor: &SidecarSupervisor,
    registry: &HookRegistry,
    db: &Database,
    now_unix: i64,
) -> Result<Option<Arc<PythonSandboxService>>, WireError> {
    // Look up the supervisor-published port for the kernel-gateway
    // sidecar. Returns None if:
    //   - the plugin isn't installed (no [[services]] entry)
    //   - the sidecar is still spawning (first reconcile pass
    //     hasn't published the port yet)
    //   - the sidecar is crash-looping (no healthy handle)
    let Some(port) = supervisor.host_port_for(SIDECAR_NAME).await else {
        tracing::warn!(
            "python_sandbox: kernel-gateway sidecar not ready; python.* tools \
             will be unavailable this boot. Install the plugin or check supervisor logs."
        );
        return Ok(None);
    };

    let gateway_url = format!("http://127.0.0.1:{port}");
    tracing::info!(%gateway_url, "python_sandbox: wiring service");

    // Per-(plugin, sidecar) state root — matches the supervisor's
    // bind-mount source for `state://work`.
    let sidecar_state =
        crate::sidecar_supervisor::plugin_state_root(PLUGIN_ID).join(SIDECAR_NAME);
    let work_root = sidecar_state.join("work");
    // Artifacts root: shared with the rest of execlaw, env-overridable.
    let artifacts_root = crate::host_caps_impl::builtin_artifacts_root_path();

    let service = PythonSandboxService::new(gateway_url, work_root, artifacts_root, db.clone())?;

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

// ===================================================================
// Tests — offline checks only. Real boot is in cli/main.rs; the
// "service constructs against a live port" path is covered by the
// live_execute_tool_round_trip integration test in tools.rs.
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_name_matches_manifest() {
        // Pin the constant against the manifest. If anyone renames
        // the [[services]] entry in plugin.toml, this test breaks
        // FIRST instead of at boot time on an operator's machine.
        let manifest = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("plugins/python-sandbox/plugin.toml"),
        )
        .expect("plugin.toml readable");
        // The manifest declares the service inline:
        //   [[services]]
        //   name = "kernel-gateway"
        let needle = format!("name = \"{SIDECAR_NAME}\"");
        assert!(
            manifest.contains(&needle),
            "plugin.toml must contain `{needle}`; if the service was \
             renamed, update SIDECAR_NAME"
        );
    }
}
