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
    PLUGIN_ID, PythonSandboxService, PythonSandboxSettings, ServiceError,
};
use crate::python_sandbox::tools::python_sandbox_tools;
use crate::sidecar_supervisor::SidecarSupervisor;
use execlaw_core::Database;
use execlaw_plugin_host::HookRegistry;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// Operator-tunable bounds. Mirror the SPA panel's `IDLE_TIMEOUT_MIN`
/// / `IDLE_TIMEOUT_MAX` / `MAX_OUTPUT_MIN` / `MAX_OUTPUT_MAX` in
/// `plugins/python-sandbox/ui/panel.tsx` so a value rejected by
/// the panel UI is also rejected here as a second line of defense
/// against a hand-edited vault row.
const IDLE_TIMEOUT_MIN_SECS: u64 = 60;
const IDLE_TIMEOUT_MAX_SECS: u64 = 24 * 60 * 60;
const MAX_OUTPUT_MIN_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_MAX_BYTES: usize = 500 * 1024 * 1024;

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
    events: &EventBus,
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
    let sidecar_state = crate::sidecar_supervisor::plugin_state_root(PLUGIN_ID).join(SIDECAR_NAME);
    let work_root = sidecar_state.join("work");
    // Artifacts root: shared with the rest of execlaw, env-overridable.
    let artifacts_root = crate::host_caps_impl::builtin_artifacts_root_path();

    // 2026-05-18 — read operator-configured settings from the
    // plugin-settings vault. Both keys are optional: a missing
    // entry falls back to the module-level defaults
    // (DEFAULT_IDLE_TIMEOUT in kernel_pool, MAX_OUTPUT_BYTES in
    // client). Invalid values (non-numeric, out of range) log a
    // warn and fall back rather than fail the wiring — settings
    // shouldn't be a boot-blocker.
    let settings = load_settings_from_vault(db);
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

/// Load operator-tunable settings from the plugin-settings vault.
/// Both keys are optional — a fresh install with no settings
/// written returns `Default::default()` and the service falls back
/// to module-level constants.
///
/// Robustness:
///   * Vault read errors log a warn + skip the key (caller falls
///     through to defaults). We never block boot on a settings
///     load — a corrupt vault row is bad UX but should not stop
///     the agent from running.
///   * Non-UTF-8 values log warn + skip.
///   * Non-numeric values log warn + skip.
///   * Out-of-range values (below the MIN / above the MAX) log
///     warn + skip. The SPA panel enforces the same bounds, so
///     the only way to land here is a hand-edited row.
///
/// Pure-ish: takes `&Database`, no other state. Tested by
/// `loads_idle_timeout_setting_from_vault` + the related
/// rejection tests below.
pub(crate) fn load_settings_from_vault(db: &Database) -> PythonSandboxSettings {
    let store = execlaw_core::vault_row::VaultRowStore::new(db);
    PythonSandboxSettings {
        idle_timeout: read_duration_setting(
            &store,
            "kernel_idle_timeout_seconds",
            IDLE_TIMEOUT_MIN_SECS,
            IDLE_TIMEOUT_MAX_SECS,
        ),
        max_output_bytes: read_usize_setting(
            &store,
            "max_output_bytes",
            MAX_OUTPUT_MIN_BYTES,
            MAX_OUTPUT_MAX_BYTES,
        ),
    }
}

fn read_duration_setting(
    store: &execlaw_core::vault_row::VaultRowStore<'_>,
    key: &str,
    min_secs: u64,
    max_secs: u64,
) -> Option<Duration> {
    let raw = read_utf8_value(store, key)?;
    let secs: u64 = match raw.parse() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(
                key,
                value = %raw,
                error = %e,
                "python_sandbox: ignoring non-numeric setting; falling back to default"
            );
            return None;
        }
    };
    if secs < min_secs || secs > max_secs {
        tracing::warn!(
            key,
            value = secs,
            min_secs,
            max_secs,
            "python_sandbox: setting out of range; falling back to default"
        );
        return None;
    }
    Some(Duration::from_secs(secs))
}

fn read_usize_setting(
    store: &execlaw_core::vault_row::VaultRowStore<'_>,
    key: &str,
    min: usize,
    max: usize,
) -> Option<usize> {
    let raw = read_utf8_value(store, key)?;
    let n: usize = match raw.parse() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(
                key,
                value = %raw,
                error = %e,
                "python_sandbox: ignoring non-numeric setting; falling back to default"
            );
            return None;
        }
    };
    if n < min || n > max {
        tracing::warn!(
            key,
            value = n,
            min,
            max,
            "python_sandbox: setting out of range; falling back to default"
        );
        return None;
    }
    Some(n)
}

fn read_utf8_value(
    store: &execlaw_core::vault_row::VaultRowStore<'_>,
    key: &str,
) -> Option<String> {
    let raw = match store.get(Some(PLUGIN_ID), key) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(
                key,
                error = %e,
                "python_sandbox: settings vault read failed; falling back to default"
            );
            return None;
        }
    };
    match String::from_utf8(raw) {
        Ok(s) => Some(s),
        Err(_) => {
            tracing::warn!(
                key,
                "python_sandbox: setting is not valid UTF-8; falling back to default"
            );
            None
        }
    }
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

    fn open_test_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn put_setting(db: &Database, key: &str, value: &str) {
        use execlaw_core::vault_row::VaultRowStore;
        VaultRowStore::new(db)
            .put(Some(PLUGIN_ID), key, value.as_bytes(), 0)
            .unwrap();
    }

    // ----------------------------------------------------------
    // Settings load — happy path + every rejection branch.
    // Locks the contract the SPA panel relies on: a written value
    // round-trips correctly; a corrupt / out-of-range value falls
    // back to default instead of failing the wiring.
    // ----------------------------------------------------------

    #[test]
    fn loads_default_settings_when_vault_empty() {
        let db = open_test_db();
        let settings = load_settings_from_vault(&db);
        assert!(settings.idle_timeout.is_none(), "no key → None → default");
        assert!(settings.max_output_bytes.is_none());
    }

    #[test]
    fn loads_idle_timeout_setting_from_vault() {
        let db = open_test_db();
        put_setting(&db, "kernel_idle_timeout_seconds", "300");
        let settings = load_settings_from_vault(&db);
        assert_eq!(settings.idle_timeout, Some(Duration::from_secs(300)));
    }

    #[test]
    fn loads_max_output_bytes_setting_from_vault() {
        let db = open_test_db();
        put_setting(&db, "max_output_bytes", "10485760"); // 10 MiB
        let settings = load_settings_from_vault(&db);
        assert_eq!(settings.max_output_bytes, Some(10 * 1024 * 1024));
    }

    #[test]
    fn rejects_non_numeric_setting() {
        let db = open_test_db();
        put_setting(&db, "kernel_idle_timeout_seconds", "not-a-number");
        let settings = load_settings_from_vault(&db);
        assert!(
            settings.idle_timeout.is_none(),
            "non-numeric value must fall back to default, not crash boot"
        );
    }

    #[test]
    fn rejects_idle_timeout_below_minimum() {
        let db = open_test_db();
        put_setting(&db, "kernel_idle_timeout_seconds", "10"); // < 60s min
        let settings = load_settings_from_vault(&db);
        assert!(settings.idle_timeout.is_none());
    }

    #[test]
    fn rejects_idle_timeout_above_maximum() {
        let db = open_test_db();
        // 25 hours > 24h max.
        put_setting(&db, "kernel_idle_timeout_seconds", "90000");
        let settings = load_settings_from_vault(&db);
        assert!(settings.idle_timeout.is_none());
    }

    #[test]
    fn rejects_max_output_below_minimum() {
        let db = open_test_db();
        put_setting(&db, "max_output_bytes", "1024"); // < 1 MiB min
        let settings = load_settings_from_vault(&db);
        assert!(settings.max_output_bytes.is_none());
    }

    #[test]
    fn rejects_max_output_above_maximum() {
        let db = open_test_db();
        // 600 MB > 500 MB max
        put_setting(&db, "max_output_bytes", "629145600");
        let settings = load_settings_from_vault(&db);
        assert!(settings.max_output_bytes.is_none());
    }

    #[test]
    fn accepts_boundary_values() {
        // Both endpoints of the allowed range must round-trip
        // (closed interval).
        let db = open_test_db();
        put_setting(&db, "kernel_idle_timeout_seconds", "60");
        put_setting(&db, "max_output_bytes", "1048576");
        let s = load_settings_from_vault(&db);
        assert_eq!(s.idle_timeout, Some(Duration::from_secs(60)));
        assert_eq!(s.max_output_bytes, Some(1024 * 1024));

        put_setting(&db, "kernel_idle_timeout_seconds", "86400");
        put_setting(&db, "max_output_bytes", "524288000");
        let s = load_settings_from_vault(&db);
        assert_eq!(s.idle_timeout, Some(Duration::from_secs(86400)));
        assert_eq!(s.max_output_bytes, Some(500 * 1024 * 1024));
    }

    #[test]
    fn manifest_declares_ui_panel() {
        // The SPA's gear icon + the DynamicPluginPanel runtime fetch
        // depend on `[[ui_panels]]` being present in plugin.toml.
        // Pin the manifest field so an accidental removal breaks
        // tests FIRST instead of silently hiding the config UI from
        // operators.
        let manifest = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("plugins/python-sandbox/plugin.toml"),
        )
        .expect("read manifest");
        assert!(
            manifest.contains("[[ui_panels]]"),
            "manifest must declare a UI panel so has_settings_ui=true",
        );
        assert!(
            manifest.contains("mount = \"admin/plugins/python-sandbox\""),
            "panel mount path must match the SPA's router expectation",
        );
        assert!(
            manifest.contains("entry = \"ui/panel.js\""),
            "panel entry must point at the built JS asset",
        );
    }

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
