//! Settings → Sidecars — operator-facing view of the sidecar
//! supervisor's per-sidecar state.
//!
//! Mirrors the `runners_admin` shape: a single `GET /api/admin/sidecars`
//! returning every registered sidecar's runtime status (Stopped /
//! Starting / Healthy / CrashLooping), plus a `POST
//! /api/admin/sidecars/:name/reset` that lets a Controller clear a
//! parked CrashLooping slot after fixing the underlying issue.
//!
//! No restart endpoint yet — the supervisor's reconcile-loop already
//! restarts on its own; the only thing operators need is the "I fixed
//! it, give it another chance" reset, which is what
//! `SidecarSupervisor::reset_attempts` is for. (Phase 3 work will add
//! a `POST /restart` for the operator-driven "force-respawn-now" flow.)

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::sidecar_supervisor::SidecarRuntimeStatus;
use crate::state::AppState;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;

/// Single sidecar's status row, as the SPA expects it. Shape mirrors
/// `SidecarRuntimeStatus` but stringifies the `ServiceStatus` so the
/// SPA doesn't need to learn the enum's serde tag.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct SidecarView {
    /// The sidecar's globally-unique name (manifest's
    /// `[[services]].name` for the entry that carried
    /// `[services.sidecar]`).
    pub name: String,
    pub plugin_id: String,
    /// Lower-cased status discriminant: `stopped` / `starting` /
    /// `healthy` / `crash_looping` / `pulling` / `not_found`. Keeps
    /// the SPA's badge mapping a flat string match.
    pub status: String,
    /// Restart attempts since the last `Healthy`. Surfaced so the
    /// SPA can render a `(3/5)` chip when the sidecar is mid-recovery.
    pub restart_attempts: u32,
    /// Loopback URL the supervisor would dispatch RPC against,
    /// `None` until the first successful spawn (when we know the
    /// host port).
    pub rpc_url: Option<String>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct SidecarListResponse {
    pub sidecars: Vec<SidecarView>,
}

/// `GET /api/admin/sidecars` — every registered sidecar's current
/// state. Open to any authed user (matches `/runners/groups`'s
/// posture); the operator-action endpoints below are
/// Controller-only.
#[utoipa::path(
    get,
    path = "/api/admin/sidecars",
    responses(
        (status = 200, description = "Live sidecar registry + runtime status",
                       body = SidecarListResponse),
        (status = 503, description = "Sidecar supervisor not configured"),
    ),
    security(("bearer_jwt" = [])),
    tag = "sidecars"
)]
pub async fn list_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<SidecarListResponse>, ApiError> {
    let Some(supervisor) = state.sidecar_supervisor.as_ref() else {
        return Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "sidecars_disabled",
            message: "sidecar supervisor not configured".into(),
        });
    };
    let snapshot = supervisor.snapshot_status().await;
    let sidecars: Vec<SidecarView> = snapshot.into_iter().map(view_from_status).collect();
    Ok(Json(SidecarListResponse { sidecars }))
}

/// `POST /api/admin/sidecars/:name/reset` — clear a parked
/// `CrashLooping` slot's restart counter and let the next reconcile
/// re-attempt the spawn. Controller-only because the operator is
/// asserting "I fixed the underlying issue."
#[utoipa::path(
    post,
    path = "/api/admin/sidecars/{name}/reset",
    params(("name" = String, Path, description = "sidecar name")),
    responses(
        (status = 200, description = "Restart counter cleared"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "No sidecar registered with this name"),
        (status = 503, description = "Sidecar supervisor not configured"),
    ),
    security(("bearer_jwt" = [])),
    tag = "sidecars"
)]
pub async fn reset_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(name): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let Some(supervisor) = state.sidecar_supervisor.as_ref() else {
        return Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "sidecars_disabled",
            message: "sidecar supervisor not configured".into(),
        });
    };
    // Validate the name exists in the snapshot — `reset_attempts`
    // would silently no-op on an unknown name and that's the wrong
    // UX (operator pasting the wrong name should see 404, not a
    // 200 they can't act on).
    let snapshot = supervisor.snapshot_status().await;
    if !snapshot.iter().any(|s| s.name == name) {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "sidecar_not_registered",
            message: format!("no sidecar registered with name '{name}'"),
        });
    }
    supervisor.reset_attempts(&name).await;
    // Kick the supervisor so the operator sees the retry land
    // within ~1s instead of waiting the full tick interval.
    supervisor.kick();
    let _ = execlaw_core::audit::AuditStore::new(&state.db).insert(
        &user.user_id,
        "sidecar_supervisor",
        &name,
        None,
        Some(&serde_json::json!({"action": "reset"})),
    );
    Ok(StatusCode::OK)
}

/// Convert the supervisor's runtime status into the SPA-facing view.
/// Pulled out so both the list handler + future per-sidecar detail
/// handler can share the conversion.
fn view_from_status(s: SidecarRuntimeStatus) -> SidecarView {
    use execlaw_container_manager::ServiceStatus;
    let status = match s.status {
        ServiceStatus::Pulling => "pulling",
        ServiceStatus::Starting => "starting",
        ServiceStatus::Healthy => "healthy",
        ServiceStatus::CrashLooping { .. } => "crash_looping",
        ServiceStatus::Stopped => "stopped",
        ServiceStatus::NotFound => "not_found",
    }
    .to_owned();
    SidecarView {
        name: s.name,
        plugin_id: s.plugin_id,
        status,
        restart_attempts: s.restart_attempts,
        rpc_url: s.rpc_url,
    }
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
            message: "only a Controller can reset sidecar attempts".into(),
        }),
    }
}

pub fn sidecars_admin_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/sidecars", get(list_handler))
        .route("/api/admin/sidecars/{name}/reset", post(reset_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use crate::sidecar_supervisor::SidecarSupervisor;
    use axum::body::{self, Body};
    use axum::http::{Method, Request, header};
    use execlaw_container_manager::MockServiceController;
    use execlaw_plugin_sdk::PluginManifest;
    use std::sync::Arc;
    use tower::ServiceExt;

    /// Mirror of the helper in `runners_admin`'s tests — bootstraps
    /// the controller account via the public `/api/setup` route so
    /// the JWT comes back signed by the same secret the AppState
    /// uses. Inlined to avoid threading visibility through.
    async fn setup_token(app: &axum::Router) -> String {
        let body = serde_json::to_vec(&serde_json::json!({
            "username": "ctrl",
            "admin_password": "hunter2-longer",
            "display_name": "Ctrl",
        }))
        .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/setup")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        v["access_token"].as_str().unwrap().to_owned()
    }

    /// Build an AppState with a SidecarSupervisor wired against a
    /// MockServiceController + one sidecar (named `signal-cli`)
    /// registered in the plugin-host registry. Returns (state,
    /// mock_handle, supervisor) so individual tests can drive
    /// `reconcile_once()` before building the router.
    fn make_state_with_sidecar() -> (AppState, Arc<MockServiceController>, SidecarSupervisor) {
        let mut state = test_app_state();
        let mock = Arc::new(MockServiceController::new());
        let registry = state.plugin_host.registry().clone();
        let m = PluginManifest::parse(
            r#"
[plugin]
id = "p-signal"
name = "P"
version = "0.1.0"

[[services]]
name = "signal-cli"
image = "execlaw/signal-cli:0.1"

[services.sidecar]
rpc_port = 8080
"#,
        )
        .unwrap();
        registry.enable(&m).unwrap();
        let supervisor = SidecarSupervisor::new(mock.clone(), registry);
        state.sidecar_supervisor = Some(supervisor.clone());
        (state, mock, supervisor)
    }

    #[tokio::test]
    async fn list_returns_503_when_supervisor_not_configured() {
        // Default test_app_state() has sidecar_supervisor: None.
        let app = build_router(test_app_state());
        let tok = setup_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/sidecars")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn list_returns_registered_sidecar_in_stopped_state_before_first_reconcile() {
        let (state, _mock, _sup) = make_state_with_sidecar();
        let app = build_router(state);
        let tok = setup_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/sidecars")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let arr = v["sidecars"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "signal-cli");
        assert_eq!(arr[0]["plugin_id"], "p-signal");
        assert_eq!(arr[0]["status"], "stopped");
        assert_eq!(arr[0]["restart_attempts"], 0);
        assert!(arr[0]["rpc_url"].is_null());
    }

    #[tokio::test]
    async fn list_reports_starting_after_first_reconcile() {
        // Drive one reconcile pass to flip status from Stopped to
        // Starting + populate the rpc_url. Pin the port (8501)
        // so a future port-pool change has to update this test.
        let (state, _mock, sup) = make_state_with_sidecar();
        sup.reconcile_once().await;
        let app = build_router(state);
        let tok = setup_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/sidecars")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let arr = v["sidecars"].as_array().unwrap();
        assert_eq!(arr[0]["status"], "starting");
        assert_eq!(arr[0]["rpc_url"], "http://127.0.0.1:8501");
    }

    #[tokio::test]
    async fn list_requires_auth() {
        let (state, _mock, _sup) = make_state_with_sidecar();
        let app = build_router(state);
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/sidecars")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // 401 Unauthorized — the auth middleware should reject before
        // we get anywhere near the handler.
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn reset_returns_404_for_unknown_name() {
        let (state, _mock, _sup) = make_state_with_sidecar();
        let app = build_router(state);
        let tok = setup_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/sidecars/never-registered/reset")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn reset_succeeds_for_registered_name() {
        let (state, _mock, _sup) = make_state_with_sidecar();
        let app = build_router(state);
        let tok = setup_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/sidecars/signal-cli/reset")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn reset_returns_503_when_supervisor_not_configured() {
        let app = build_router(test_app_state());
        let tok = setup_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/sidecars/signal-cli/reset")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
