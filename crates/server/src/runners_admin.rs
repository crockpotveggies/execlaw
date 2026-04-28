//! Settings → Runners — view-only surface for live runner state +
//! a force-restart action (Phase 8.5).
//!
//! Per the architecture (`docs/runner-design.md`), runners are
//! managed automatically by the control plane, one per
//! conversation, with the controller's runner kept hot
//! indefinitely and others reaped after 10 minutes idle. The
//! operator never *creates* a runner; they only observe and, in
//! rare cases, force-restart a stuck one.

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::runner_registry::{RunnerEntry, RunnerModality};
use crate::state::AppState;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct RunnerView {
    pub conversation_id: String,
    pub principal_label: Option<String>,
    pub modality: String,
    pub controller_runner: bool,
    pub started_at: i64,
    pub last_active_at: i64,
    pub in_flight: bool,
    pub turn_count: u64,
    pub restart_pending: bool,
    /// Seconds remaining before the idle reaper drops this entry.
    /// `None` for the controller runner (never reaped) and for any
    /// in-flight runner (reaper skips busy entries).
    pub idle_secs_remaining: Option<i64>,
}

impl RunnerView {
    fn from_entry(entry: &RunnerEntry) -> Self {
        let idle_secs_remaining =
            if entry.controller_runner || entry.in_flight {
                None
            } else {
                let elapsed = chrono::Utc::now()
                    .signed_duration_since(entry.last_active_at)
                    .num_seconds();
                let ttl = crate::runner_registry::IDLE_TTL.as_secs() as i64;
                Some((ttl - elapsed).max(0))
            };
        Self {
            conversation_id: entry.conversation_id.clone(),
            principal_label: entry.principal_label.clone(),
            modality: match entry.modality {
                RunnerModality::Text => "Text",
                RunnerModality::Voice => "Voice",
            }
            .to_owned(),
            controller_runner: entry.controller_runner,
            started_at: entry.started_at.timestamp(),
            last_active_at: entry.last_active_at.timestamp(),
            in_flight: entry.in_flight,
            turn_count: entry.turn_count,
            restart_pending: entry.restart_pending,
            idle_secs_remaining,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RunnerListResponse {
    pub runners: Vec<RunnerView>,
    /// Idle TTL the reaper applies to non-controller runners. Surfaced so
    /// the SPA can label the row's countdown without baking the value in.
    pub idle_ttl_secs: i64,
}

#[utoipa::path(
    get,
    path = "/api/admin/runners",
    responses((status = 200, description = "Live runners", body = RunnerListResponse)),
    security(("bearer_jwt" = [])),
    tag = "runners"
)]
pub async fn list_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<RunnerListResponse>, ApiError> {
    let runners: Vec<RunnerView> = state
        .runner_registry
        .snapshot()
        .iter()
        .map(RunnerView::from_entry)
        .collect();
    Ok(Json(RunnerListResponse {
        runners,
        idle_ttl_secs: crate::runner_registry::IDLE_TTL.as_secs() as i64,
    }))
}

#[utoipa::path(
    post,
    path = "/api/admin/runners/{conversation_id}/restart",
    params(("conversation_id" = String, Path, description = "Conversation whose runner should be force-restarted")),
    responses(
        (status = 200, description = "Restart signalled"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "No runner registered for this conversation"),
    ),
    security(("bearer_jwt" = [])),
    tag = "runners"
)]
pub async fn restart_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(conversation_id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    if !state.runner_registry.restart(&conversation_id) {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "runner_not_registered",
            message: format!(
                "no runner is currently tracked for conversation '{conversation_id}'"
            ),
        });
    }
    let _ = execlaw_core::audit::AuditStore::new(&state.db).insert(
        &user.user_id,
        "state_runners",
        &conversation_id,
        None,
        Some(&serde_json::json!({"action": "restart"})),
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

// ---------------------------------------------------------------------------
// Phase 16 — supervisor-driven endpoints (per principal_group, not per
// conversation). These run alongside the legacy runner_registry endpoints
// while the cutover settles. Once `runner_supervisor` becomes the only
// runner concept, the conversation-keyed endpoints above can be removed.
// ---------------------------------------------------------------------------

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
        .reap_group(&group_id, execlaw_runner_protocol::ShutdownReason::OperatorRestart)
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
        .reap_group(&group_id, execlaw_runner_protocol::ShutdownReason::OperatorWipe)
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

pub fn runners_admin_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/runners", get(list_handler))
        .route(
            "/api/admin/runners/{conversation_id}/restart",
            post(restart_handler),
        )
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
    use crate::runner_registry::RunnerModality;
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
    async fn list_returns_seeded_runners_in_order() {
        let state = test_app_state();
        state.runner_registry.register_turn_start(
            "conv-old",
            Some("Alice".into()),
            RunnerModality::Text,
            false,
        );
        state.runner_registry.register_turn_end("conv-old");
        state.runner_registry.register_turn_start(
            "conv-busy",
            Some("Controller".into()),
            RunnerModality::Text,
            true,
        );
        let app = build_router(state);
        let tok = setup_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/runners")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let arr = v["runners"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["conversation_id"], "conv-busy", "in-flight first");
        assert_eq!(arr[0]["controller_runner"], true);
        assert!(arr[0]["idle_secs_remaining"].is_null(), "controller never idles out");
    }

    #[tokio::test]
    async fn restart_unknown_returns_404() {
        let app = build_router(test_app_state());
        let tok = setup_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/runners/never-saw-it/restart")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn restart_round_trip_marks_pending_in_subsequent_list() {
        let state = test_app_state();
        state.runner_registry.register_turn_start(
            "conv-1",
            None,
            RunnerModality::Text,
            false,
        );
        let app = build_router(state.clone());
        let tok = setup_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/runners/conv-1/restart")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // List should show restart_pending = true.
        let snap = state.runner_registry.snapshot();
        assert_eq!(snap.len(), 1);
        assert!(snap[0].restart_pending);
        assert!(!snap[0].in_flight);
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
        let _ = supervisor.accept_registration("g-test", &sec, true).unwrap();
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
        state.runner_supervisor = Some(
            crate::runner_supervisor::RunnerSupervisor::new(
                state.db.clone(),
                state.events.clone(),
            ),
        );
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
            .uri("/api/admin/runners")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
