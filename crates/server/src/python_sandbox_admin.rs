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
//!     error. Changes take effect on the next server restart —
//!     same "applies on next restart" UX the previous plugin
//!     settings panel had.
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
    Ok(Json(updated.into()))
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
