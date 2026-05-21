//! 2026-05-20 — admin routes for the python-sandbox NATIVE feature
//! (formerly a plugin, migrated to native in commit `<this one>`).
//!
//! Two endpoints, both Controller-only:
//!
//!   * `GET  /api/admin/python-sandbox` — returns the current config
//!     row (enabled toggle + tunables) plus live status (sidecar
//!     state + Docker availability).
//!   * `PUT  /api/admin/python-sandbox` — updates any subset of the
//!     config row. Operator-supplied tunables are bounds-checked
//!     server-side; out-of-range returns 400 with a descriptive
//!     error. The `enabled` toggle takes effect **immediately**:
//!     enable=true registers the native sidecar with the registry,
//!     kicks the supervisor, and spawns a background task that
//!     wires the python.* tool surface once the kernel-gateway is
//!     healthy; enable=false drops the python.* tools + the
//!     sidecar slot from the registry, and the supervisor's next
//!     reconcile stops the container. No server restart required.
//!
//! Status surface: when the python-sandbox feature is on AND
//! Docker is reachable, the sidecar's runtime state is included
//! so the SPA Settings page can render the same chip operators
//! see on the Sidecars admin page. When the feature is off, the
//! sidecar block in the response is omitted; the SPA hides the
//! status row in that mode.

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::sidecar_supervisor::SidecarSupervisor;
use crate::state::AppState;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use execlaw_core::python_sandbox_config::{
    PythonSandboxConfig, PythonSandboxConfigStore, PythonSandboxConfigUpdate,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// Mirror the wiring layer's bounds — out-of-range from the API
// gets rejected with a clear error rather than silently clamped
// at boot. Same numbers as the previous plugin config panel's
// client-side bounds.
const IDLE_TIMEOUT_MIN_SECS: u32 = 60;
const IDLE_TIMEOUT_MAX_SECS: u32 = 24 * 60 * 60;
const MAX_OUTPUT_MIN_BYTES: u64 = 1024 * 1024;
const MAX_OUTPUT_MAX_BYTES: u64 = 500 * 1024 * 1024;

#[derive(Debug, Serialize, ToSchema)]
pub struct PythonSandboxStatusResponse {
    /// Operator-editable config row (enabled toggle + tunables).
    pub config: PythonSandboxConfigView,
    /// Whether Docker is reachable on this host. When `false`, the
    /// SPA Settings page disables the enable toggle and shows a
    /// "Docker not detected" hint — the operator can't turn the
    /// feature on without a working Docker daemon.
    pub docker_available: bool,
    /// Sidecar runtime state. `None` when the feature is disabled
    /// (no sidecar registered) or when the supervisor is itself
    /// disabled (no Docker). Otherwise mirrors what
    /// `GET /api/admin/sidecars` returns for the kernel-gateway
    /// entry.
    pub sidecar: Option<SidecarStatusView>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PythonSandboxConfigView {
    pub enabled: bool,
    pub idle_timeout_seconds: u32,
    pub max_output_bytes: u64,
    pub updated_at: i64,
}

impl From<PythonSandboxConfig> for PythonSandboxConfigView {
    fn from(c: PythonSandboxConfig) -> Self {
        Self {
            enabled: c.enabled,
            idle_timeout_seconds: c.idle_timeout_seconds,
            max_output_bytes: c.max_output_bytes,
            updated_at: c.updated_at,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SidecarStatusView {
    pub status: String,
    pub rpc_url: Option<String>,
    pub restart_attempts: u32,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdatePythonSandboxRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub idle_timeout_seconds: Option<u32>,
    #[serde(default)]
    pub max_output_bytes: Option<u64>,
}

#[utoipa::path(
    get,
    path = "/api/admin/python-sandbox",
    responses(
        (status = 200, description = "Current config + live status", body = PythonSandboxStatusResponse),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "python-sandbox"
)]
pub async fn get_handler(
    State(state): State<AppState>,
    user: AuthedUser,
) -> Result<Json<PythonSandboxStatusResponse>, ApiError> {
    require_controller(&state, &user)?;
    let config = PythonSandboxConfigStore::new(&state.db)
        .get()
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "db_error",
            message: e.to_string(),
        })?;
    let docker_available = state.sidecar_supervisor.is_some();
    let sidecar = match state.sidecar_supervisor.as_ref() {
        Some(sup) if config.enabled => fetch_sidecar_view(sup).await,
        _ => None,
    };
    Ok(Json(PythonSandboxStatusResponse {
        config: config.into(),
        docker_available,
        sidecar,
    }))
}

#[utoipa::path(
    put,
    path = "/api/admin/python-sandbox",
    request_body = UpdatePythonSandboxRequest,
    responses(
        (status = 200, description = "Updated config", body = PythonSandboxConfigView),
        (status = 400, description = "Invalid tunable value (out of range)"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "python-sandbox"
)]
pub async fn put_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<UpdatePythonSandboxRequest>,
) -> Result<Json<PythonSandboxConfigView>, ApiError> {
    require_controller(&state, &user)?;

    // Validate operator-supplied tunables. Boundaries match the
    // wiring layer's clamp + the previous plugin panel's bounds.
    if let Some(secs) = req.idle_timeout_seconds {
        if !(IDLE_TIMEOUT_MIN_SECS..=IDLE_TIMEOUT_MAX_SECS).contains(&secs) {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_idle_timeout",
                message: format!(
                    "idle_timeout_seconds must be {IDLE_TIMEOUT_MIN_SECS}..={IDLE_TIMEOUT_MAX_SECS}; got {secs}"
                ),
            });
        }
    }
    if let Some(bytes) = req.max_output_bytes {
        if !(MAX_OUTPUT_MIN_BYTES..=MAX_OUTPUT_MAX_BYTES).contains(&bytes) {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_max_output",
                message: format!(
                    "max_output_bytes must be {MAX_OUTPUT_MIN_BYTES}..={MAX_OUTPUT_MAX_BYTES}; got {bytes}"
                ),
            });
        }
    }
    // Guard: if operator tries to enable but Docker is missing,
    // refuse explicitly rather than silently auto-disabling on
    // the next boot. Same predicate as the boot wiring.
    if req.enabled == Some(true) && state.sidecar_supervisor.is_none() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "docker_unavailable",
            message: "Cannot enable python-sandbox: Docker is not detected on this host. \
                      Install Docker Desktop (or equivalent) and restart execlaw."
                .into(),
        });
    }

    let now = chrono::Utc::now().timestamp();
    let updated = PythonSandboxConfigStore::new(&state.db)
        .update(
            PythonSandboxConfigUpdate {
                enabled: req.enabled,
                idle_timeout_seconds: req.idle_timeout_seconds,
                max_output_bytes: req.max_output_bytes,
            },
            now,
        )
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "db_error",
            message: e.to_string(),
        })?;

    // Live-toggle lifecycle. The DB write above is the durable
    // source of truth; this block makes the change take effect
    // RIGHT NOW so the operator doesn't have to restart. Idle
    // toggles (no change to `enabled`) skip this entirely.
    if let Some(new_enabled) = req.enabled {
        apply_enabled_change(&state, new_enabled);
    }

    Ok(Json(updated.into()))
}

/// Drive the side effects of the `enabled` toggle: register /
/// unregister the sidecar with the host registry, kick the
/// supervisor to reconcile immediately, and (on enable) spawn a
/// background task that wires the python.* tool surface once the
/// kernel-gateway is healthy.
///
/// Best-effort: failures inside this function are logged but DO
/// NOT propagate up to the HTTP response. The DB row was already
/// updated, so the boot path will pick up the toggle next time
/// even if this live attempt didn't fully succeed.
fn apply_enabled_change(state: &AppState, new_enabled: bool) {
    let Some(supervisor) = state.sidecar_supervisor.clone() else {
        // Docker unavailable → boot path didn't construct a
        // supervisor → nothing for us to drive here. The validator
        // above already rejected enable=true in this branch, so
        // we only reach this with enable=false on a Docker-less
        // host, which is a clean no-op.
        return;
    };
    let registry = state.plugin_host.registry().clone();
    if new_enabled {
        match crate::python_sandbox::register_now(&registry) {
            Ok(_) => {
                supervisor.kick();
            }
            Err(e) => {
                tracing::warn!(
                    target: "python_sandbox::live_toggle",
                    error = %e,
                    "live register failed; operator may need to restart to fully wire the feature"
                );
                return;
            }
        }
        // The kernel-gateway container takes ~3-10s to start +
        // become responsive. Spawn a background task that polls
        // `host_port_for` until populated then wires the tools.
        // Returns Ok(None) silently when the feature is somehow
        // disabled mid-flight (operator toggled twice fast).
        let db = state.db.clone();
        let events = state.events.clone();
        let sup = supervisor.clone();
        let registry_for_wire = registry.clone();
        tokio::spawn(async move {
            // Wait up to 60s for the kernel-gateway to surface a
            // host port. 60s is conservative — the legacy boot path
            // historically waited ~2s. Container pull + first-boot
            // initialization on a cold host can stretch longer.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                if sup
                    .host_port_for(crate::python_sandbox::wiring::SIDECAR_NAME)
                    .await
                    .is_some()
                {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    tracing::warn!(
                        target: "python_sandbox::live_toggle",
                        "kernel-gateway sidecar didn't surface a host port within 60s; \
                         tools NOT wired. Operator can re-toggle to retry, or check supervisor logs."
                    );
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            let now_unix = chrono::Utc::now().timestamp();
            match crate::python_sandbox::wire_python_sandbox(
                &sup,
                &registry_for_wire,
                &db,
                &events,
                now_unix,
            )
            .await
            {
                Ok(Some(_)) => {
                    tracing::info!(
                        target: "python_sandbox::live_toggle",
                        "python.* tools wired against live kernel-gateway after toggle-on"
                    );
                }
                Ok(None) => {
                    tracing::warn!(
                        target: "python_sandbox::live_toggle",
                        "wire_python_sandbox returned None — config may have flipped back to disabled mid-wire"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "python_sandbox::live_toggle",
                        error = ?e,
                        "wire_python_sandbox failed after live toggle-on"
                    );
                }
            }
        });
    } else {
        crate::python_sandbox::unregister_now(&registry);
        supervisor.kick();
    }
}

/// Look up the kernel-gateway sidecar's runtime status from the
/// supervisor's snapshot. Returns `None` when no slot is registered
/// (feature disabled, native registration didn't fire at boot).
async fn fetch_sidecar_view(sup: &SidecarSupervisor) -> Option<SidecarStatusView> {
    let snapshot = sup.snapshot_status().await;
    let found = snapshot
        .into_iter()
        .find(|s| s.name == crate::python_sandbox::wiring::SIDECAR_NAME)?;
    Some(SidecarStatusView {
        // Lowercase discriminant — matches the existing
        // `/api/admin/sidecars` projection so the SPA chip-mapping
        // table doesn't need a python-sandbox-specific branch.
        status: format!("{:?}", found.status).to_lowercase(),
        rpc_url: found.rpc_url,
        restart_attempts: found.restart_attempts,
    })
}

fn require_controller(state: &AppState, user: &AuthedUser) -> Result<(), ApiError> {
    use execlaw_core::users::{UserRole, UserStore};
    let row = UserStore::new(&state.db)
        .get_by_id(&user.user_id)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "db_error",
            message: e.to_string(),
        })?;
    match row.map(|u| u.role) {
        Some(UserRole::Controller) => Ok(()),
        _ => Err(ApiError {
            status: StatusCode::FORBIDDEN,
            code: "controller_only",
            message: "only a Controller can change python-sandbox settings".into(),
        }),
    }
}

pub fn python_sandbox_admin_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/python-sandbox", get(get_handler).put(put_handler))
}
