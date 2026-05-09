//! Settings → Runners — view + operator actions.
//!
//! Per the architecture (`docs/runner-design.md`), runners are managed
//! automatically by the control plane: one container per
//! `(channel, principals)` group, hot for ~10 min idle, except the
//! Controller's runner which stays hot indefinitely.
//!
//! This module exposes the supervisor-driven admin surface
//! (`/api/admin/runners/groups[...]`). The legacy per-conversation
//! `/api/admin/runners` endpoints (and the in-process `runner_registry`
//! that backed them) were retired on 2026-04-28; the supervisor is the
//! single source of truth.

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct GroupRunnerView {
    pub group_id: String,
    pub controller_runner: bool,
    pub status: String,
    pub started_at: i64,
    pub last_active_at: i64,
    pub in_flight_turns: u32,
    pub container_id: Option<String>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct GroupRunnerListResponse {
    pub runners: Vec<GroupRunnerView>,
    pub idle_ttl_secs: i64,
}

/// `GET /api/admin/runners/groups` — supervisor-tracked runners.
#[utoipa::path(
    get,
    path = "/api/admin/runners/groups",
    responses(
        (status = 200, description = "Live group runners", body = GroupRunnerListResponse),
        (status = 503, description = "Runner supervisor not configured"),
    ),
    security(("bearer_jwt" = [])),
    tag = "runners"
)]
pub async fn list_groups_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<GroupRunnerListResponse>, ApiError> {
    let Some(supervisor) = state.runner_supervisor.as_ref() else {
        return Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "runners_disabled",
            message: "runner supervisor not configured".into(),
        });
    };
    let snapshot = supervisor.snapshot();
    let mut views: Vec<GroupRunnerView> = Vec::with_capacity(snapshot.len());
    for handle in snapshot {
        let s = handle.state.read().await;
        views.push(GroupRunnerView {
            group_id: handle.group_id.clone(),
            controller_runner: handle.controller_runner,
            status: format!("{:?}", s.status).to_lowercase(),
            started_at: s.started_at.timestamp(),
            last_active_at: s.last_active_at.timestamp(),
            in_flight_turns: s.in_flight_turns.len() as u32,
            container_id: s.container_id.clone(),
        });
    }
    views.sort_by(|a, b| {
        b.in_flight_turns
            .cmp(&a.in_flight_turns)
            .then_with(|| b.last_active_at.cmp(&a.last_active_at))
            .then_with(|| a.group_id.cmp(&b.group_id))
    });
    Ok(Json(GroupRunnerListResponse {
        runners: views,
        idle_ttl_secs: crate::runner_supervisor::IDLE_TTL.as_secs() as i64,
    }))
}

/// `POST /api/admin/runners/groups/:group_id/restart` — operator-
/// driven runner restart. Preserves the workspace volume; the next
/// turn re-spawns onto the same scratch.
#[utoipa::path(
    post,
    path = "/api/admin/runners/groups/{group_id}/restart",
    params(("group_id" = String, Path, description = "principal group id")),
    responses(
        (status = 200, description = "Restart signalled"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "No runner registered for this group"),
        (status = 503, description = "Runner supervisor not configured"),
    ),
    security(("bearer_jwt" = [])),
    tag = "runners"
)]
pub async fn restart_group_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(group_id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let Some(supervisor) = state.runner_supervisor.as_ref() else {
        return Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "runners_disabled",
            message: "runner supervisor not configured".into(),
        });
    };
    if supervisor.get(&group_id).is_none() {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "runner_not_registered",
            message: format!("no runner registered for group '{group_id}'"),
        });
    }
    // Send the polite Shutdown frame; the operator's launcher (CLI
    // boot wires it) handles the kill. Volume is PRESERVED — that's
    // the contract for OperatorRestart.
    let _ = supervisor
        .reap_group(
            &group_id,
            execlaw_runner_protocol::ShutdownReason::OperatorRestart,
        )
        .await;
    let _ = execlaw_core::audit::AuditStore::new(&state.db).insert(
        &user.user_id,
        "state_runner_groups",
        &group_id,
        None,
        Some(&serde_json::json!({"action": "restart"})),
    );
    Ok(StatusCode::OK)
}

/// `POST /api/admin/runners/groups/:group_id/wipe` — operator-driven
/// "wipe workspace" action. Kills the runner AND removes the named
/// volume. The group_id stays in `state_principal_groups`; only the
/// scratch files are gone.
#[utoipa::path(
    post,
    path = "/api/admin/runners/groups/{group_id}/wipe",
    params(("group_id" = String, Path, description = "principal group id")),
    responses(
        (status = 200, description = "Wipe signalled"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "No runner registered for this group"),
        (status = 503, description = "Runner supervisor not configured"),
    ),
    security(("bearer_jwt" = [])),
    tag = "runners"
)]
pub async fn wipe_group_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(group_id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let Some(supervisor) = state.runner_supervisor.as_ref() else {
        return Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "runners_disabled",
            message: "runner supervisor not configured".into(),
        });
    };
    if supervisor.get(&group_id).is_none() {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "runner_not_registered",
            message: format!("no runner registered for group '{group_id}'"),
        });
    }
    // Wipe goes through the WS-level dance only; the bollard
    // launcher hookup lives in the cli's `cmd_serve` (it owns the
    // launcher instance). The supervisor's WS-only `reap_group`
    // sends the right Shutdown reason; the cli's reaper picks it
    // up + does the kill + volume rm.
    let _ = supervisor
        .reap_group(
            &group_id,
            execlaw_runner_protocol::ShutdownReason::OperatorWipe,
        )
        .await;
    let _ = execlaw_core::audit::AuditStore::new(&state.db).insert(
        &user.user_id,
        "state_runner_groups",
        &group_id,
        None,
        Some(&serde_json::json!({"action": "wipe"})),
    );
    Ok(StatusCode::OK)
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
            message: "only a Controller can restart runners".into(),
        }),
    }
}

pub fn runners_admin_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/runners/groups", get(list_groups_handler))
        .route(
            "/api/admin/runners/groups/{group_id}/restart",
            post(restart_group_handler),
        )
        .route(
            "/api/admin/runners/groups/{group_id}/wipe",
            post(wipe_group_handler),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::{self, Body};
    use axum::http::{Method, Request, header};
    use tower::ServiceExt;

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

    #[tokio::test]
    async fn list_groups_returns_503_when_supervisor_disabled() {
        // Default test_app_state() has runner_supervisor: None.
        let app = build_router(test_app_state());
        let tok = setup_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/runners/groups")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn list_groups_returns_supervisor_snapshot_when_enabled() {
        let mut state = test_app_state();
        let supervisor =
            crate::runner_supervisor::RunnerSupervisor::new(state.db.clone(), state.events.clone());
        // Seed one runner via the public auth path.
        let (sec, _) = supervisor.register_pending_spawn("g-test");
        let _ = supervisor
            .accept_registration("g-test", &sec, true)
            .unwrap();
        state.runner_supervisor = Some(supervisor);

        let app = build_router(state);
        let tok = setup_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/runners/groups")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["runners"].as_array().unwrap().len(), 1);
        assert_eq!(v["runners"][0]["group_id"], "g-test");
        assert_eq!(v["runners"][0]["controller_runner"], true);
        assert_eq!(v["idle_ttl_secs"], 600);
    }

    #[tokio::test]
    async fn restart_group_returns_404_when_unknown() {
        let mut state = test_app_state();
        state.runner_supervisor = Some(crate::runner_supervisor::RunnerSupervisor::new(
            state.db.clone(),
            state.events.clone(),
        ));
        let app = build_router(state);
        let tok = setup_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/runners/groups/g-missing/restart")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn list_requires_auth() {
        let app = build_router(test_app_state());
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/runners/groups")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
