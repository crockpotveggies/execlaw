//! Settings → Routines (Phase 10, MIGRATION_PLAN §5.6).
//!
//! Operator CRUD over `config_routines` plus a manual-fire endpoint
//! and a per-routine run-history sub-resource. The scheduler tick
//! (in `routine_runner.rs`) is the actual firing path; this module
//! is purely the operator surface.
//!
//! Routes:
//!   * `GET    /api/admin/routines` — list with computed status hints
//!   * `POST   /api/admin/routines` — create
//!   * `GET    /api/admin/routines/{id}` — single row
//!   * `PUT    /api/admin/routines/{id}` — update
//!   * `DELETE /api/admin/routines/{id}` — drop (cascades runs)
//!   * `POST   /api/admin/routines/{id}/run-now` — queue an immediate fire
//!   * `GET    /api/admin/routines/{id}/runs?limit=N` — history
//!   * `POST   /api/admin/routines/preview` — render next-N fire times

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use chrono::{TimeZone, Utc};
use execlaw_core::audit::AuditStore;
use execlaw_core::routines::{
    next_n_fires, parse_cron, parse_timezone, RoutineError, RoutineRow, RoutineRunRow,
    RoutineRunStatus, RoutineStore, RoutineUpsert,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RoutineView {
    pub id: String,
    pub name: String,
    pub schedule_cron: String,
    pub timezone: String,
    pub prompt: String,
    pub target_conversation_id: Option<String>,
    pub enabled: bool,
    pub last_run_at: Option<i64>,
    /// One of "Success" | "Failed" | "Skipped" | null.
    pub last_run_status: Option<String>,
    pub next_run_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<&RoutineRow> for RoutineView {
    fn from(r: &RoutineRow) -> Self {
        Self {
            id: r.id.clone(),
            name: r.name.clone(),
            schedule_cron: r.schedule_cron.clone(),
            timezone: r.timezone.clone(),
            prompt: r.prompt.clone(),
            target_conversation_id: r.target_conversation_id.clone(),
            enabled: r.enabled,
            last_run_at: r.last_run_at,
            last_run_status: r.last_run_status.map(|s| s.as_str().to_owned()),
            next_run_at: r.next_run_at,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RoutineListResponse {
    pub routines: Vec<RoutineView>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RoutineRunView {
    pub id: String,
    pub routine_id: String,
    pub fired_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub status: String,
    pub error: Option<String>,
    pub conversation_id: Option<String>,
}

impl From<&RoutineRunRow> for RoutineRunView {
    fn from(r: &RoutineRunRow) -> Self {
        Self {
            id: r.id.clone(),
            routine_id: r.routine_id.clone(),
            fired_at: r.fired_at,
            started_at: r.started_at,
            finished_at: r.finished_at,
            status: r.status.as_str().to_owned(),
            error: r.error.clone(),
            conversation_id: r.conversation_id.clone(),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RoutineRunListResponse {
    pub runs: Vec<RoutineRunView>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpsertRoutineRequest {
    pub name: String,
    pub schedule_cron: String,
    #[serde(default = "default_tz")]
    pub timezone: String,
    pub prompt: String,
    #[serde(default)]
    pub target_conversation_id: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn default_tz() -> String {
    "UTC".into()
}
fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PreviewRequest {
    pub schedule_cron: String,
    #[serde(default = "default_tz")]
    pub timezone: String,
    /// Number of upcoming fires to compute. Capped at 10 server-side.
    #[serde(default = "default_preview_n")]
    pub n: u32,
}
fn default_preview_n() -> u32 {
    5
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PreviewResponse {
    pub next_fires_unix: Vec<i64>,
}

#[derive(Debug, Deserialize)]
pub struct RunsQuery {
    pub limit: Option<u32>,
}

impl From<RoutineError> for ApiError {
    fn from(err: RoutineError) -> Self {
        match err {
            RoutineError::Db(e) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "db_error",
                message: e.to_string(),
            },
            RoutineError::Sqlite(e) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "db_error",
                message: e.to_string(),
            },
            RoutineError::Invalid(msg) => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_routine",
                message: msg,
            },
            RoutineError::NotFound(id) => ApiError {
                status: StatusCode::NOT_FOUND,
                code: "routine_not_found",
                message: format!("routine '{id}' does not exist"),
            },
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/routines",
    responses((status = 200, description = "All routines", body = RoutineListResponse)),
    security(("bearer_jwt" = [])),
    tag = "routines"
)]
pub async fn list_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<RoutineListResponse>, ApiError> {
    let store = RoutineStore::new(&state.db);
    let rows = store.list_all().map_err(ApiError::from)?;
    Ok(Json(RoutineListResponse {
        routines: rows.iter().map(RoutineView::from).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/admin/routines",
    request_body = UpsertRoutineRequest,
    responses(
        (status = 200, description = "Created", body = RoutineView),
        (status = 400, description = "Validation failed"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "routines"
)]
pub async fn create_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<UpsertRoutineRequest>,
) -> Result<Json<RoutineView>, ApiError> {
    require_controller(&state, &user)?;
    let store = RoutineStore::new(&state.db);
    let now = chrono::Utc::now().timestamp();
    let row = store
        .upsert(
            &RoutineUpsert {
                id: None,
                name: req.name.clone(),
                schedule_cron: req.schedule_cron.clone(),
                timezone: req.timezone.clone(),
                prompt: req.prompt.clone(),
                target_conversation_id: req.target_conversation_id.clone(),
                enabled: req.enabled,
            },
            now,
        )
        .map_err(ApiError::from)?;
    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "config_routines",
        &row.id,
        None,
        Some(&serde_json::json!({
            "name": row.name,
            "schedule_cron": row.schedule_cron,
            "enabled": row.enabled,
        })),
    );
    Ok(Json((&row).into()))
}

#[utoipa::path(
    get,
    path = "/api/admin/routines/{id}",
    params(("id" = String, Path, description = "Routine id")),
    responses(
        (status = 200, description = "Single routine", body = RoutineView),
        (status = 404, description = "Routine id not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "routines"
)]
pub async fn get_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<RoutineView>, ApiError> {
    let store = RoutineStore::new(&state.db);
    let row = store
        .get(&id)
        .map_err(ApiError::from)?
        .ok_or(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "routine_not_found",
            message: format!("routine '{id}' does not exist"),
        })?;
    Ok(Json((&row).into()))
}

#[utoipa::path(
    put,
    path = "/api/admin/routines/{id}",
    request_body = UpsertRoutineRequest,
    params(("id" = String, Path, description = "Routine id")),
    responses(
        (status = 200, description = "Updated", body = RoutineView),
        (status = 400, description = "Validation failed"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "Routine id not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "routines"
)]
pub async fn update_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<UpsertRoutineRequest>,
) -> Result<Json<RoutineView>, ApiError> {
    require_controller(&state, &user)?;
    let store = RoutineStore::new(&state.db);
    let prior = store
        .get(&id)
        .map_err(ApiError::from)?
        .ok_or(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "routine_not_found",
            message: format!("routine '{id}' does not exist"),
        })?;
    let now = chrono::Utc::now().timestamp();
    let row = store
        .upsert(
            &RoutineUpsert {
                id: Some(id.clone()),
                name: req.name.clone(),
                schedule_cron: req.schedule_cron.clone(),
                timezone: req.timezone.clone(),
                prompt: req.prompt.clone(),
                target_conversation_id: req.target_conversation_id.clone(),
                enabled: req.enabled,
            },
            now,
        )
        .map_err(ApiError::from)?;
    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "config_routines",
        &id,
        Some(&serde_json::json!({
            "name": prior.name,
            "schedule_cron": prior.schedule_cron,
            "enabled": prior.enabled,
        })),
        Some(&serde_json::json!({
            "name": row.name,
            "schedule_cron": row.schedule_cron,
            "enabled": row.enabled,
        })),
    );
    Ok(Json((&row).into()))
}

#[utoipa::path(
    delete,
    path = "/api/admin/routines/{id}",
    params(("id" = String, Path, description = "Routine id")),
    responses(
        (status = 200, description = "Deleted"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "Routine id not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "routines"
)]
pub async fn delete_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let store = RoutineStore::new(&state.db);
    let prior = store.get(&id).map_err(ApiError::from)?;
    let removed = store.delete(&id).map_err(ApiError::from)?;
    if !removed {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "routine_not_found",
            message: format!("routine '{id}' does not exist"),
        });
    }
    if let Some(prior) = prior {
        let _ = AuditStore::new(&state.db).insert(
            &user.user_id,
            "config_routines",
            &id,
            Some(&serde_json::json!({"name": prior.name})),
            None,
        );
    }
    Ok(StatusCode::OK)
}

#[utoipa::path(
    post,
    path = "/api/admin/routines/{id}/run-now",
    params(("id" = String, Path, description = "Routine id")),
    responses(
        (status = 200, description = "Run queued"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "Routine id not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "routines"
)]
pub async fn run_now_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<RoutineRunView>, ApiError> {
    require_controller(&state, &user)?;
    let store = RoutineStore::new(&state.db);
    let row = store
        .get(&id)
        .map_err(ApiError::from)?
        .ok_or(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "routine_not_found",
            message: format!("routine '{id}' does not exist"),
        })?;
    let now = chrono::Utc::now().timestamp();
    let run_id = store.insert_run_pending(&row.id, now).map_err(ApiError::from)?;
    state.events.publish(crate::events::UiEvent::RoutineRunChanged {
        routine_id: row.id.clone(),
        run_id: run_id.clone(),
        status: RoutineRunStatus::Pending.as_str().to_owned(),
    });
    // Mark the run Skipped immediately for v1 — the actual dispatch
    // path lands when runner-local is real. The history row still
    // exists so the operator can see "yes, the manual-fire button
    // worked." Once dispatch is wired, this will hand off to the
    // runner instead.
    store
        .finish_run(
            &run_id,
            RoutineRunStatus::Skipped,
            now,
            Some(
                "manual run queued; dispatch pipeline lands with runner-local",
            ),
            None,
        )
        .map_err(ApiError::from)?;
    state.events.publish(crate::events::UiEvent::RoutineRunChanged {
        routine_id: row.id.clone(),
        run_id: run_id.clone(),
        status: RoutineRunStatus::Skipped.as_str().to_owned(),
    });
    let runs = store.list_runs(&row.id, 1).map_err(ApiError::from)?;
    let run = runs.first().ok_or(ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "run_lost",
        message: "freshly-inserted run row vanished".into(),
    })?;
    Ok(Json(RoutineRunView::from(run)))
}

#[utoipa::path(
    get,
    path = "/api/admin/routines/{id}/runs",
    params(
        ("id"    = String,      Path,  description = "Routine id"),
        ("limit" = Option<u32>, Query, description = "Cap on rows (default 50, max 500)"),
    ),
    responses(
        (status = 200, description = "Run history", body = RoutineRunListResponse),
        (status = 404, description = "Routine id not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "routines"
)]
pub async fn runs_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<RunsQuery>,
) -> Result<Json<RoutineRunListResponse>, ApiError> {
    let store = RoutineStore::new(&state.db);
    if store.get(&id).map_err(ApiError::from)?.is_none() {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "routine_not_found",
            message: format!("routine '{id}' does not exist"),
        });
    }
    let limit = q.limit.unwrap_or(50).min(500);
    let rows = store.list_runs(&id, limit).map_err(ApiError::from)?;
    Ok(Json(RoutineRunListResponse {
        runs: rows.iter().map(RoutineRunView::from).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/admin/routines/preview",
    request_body = PreviewRequest,
    responses(
        (status = 200, description = "Next N fire times (Unix UTC)", body = PreviewResponse),
        (status = 400, description = "Cron / tz validation failed"),
    ),
    security(("bearer_jwt" = [])),
    tag = "routines"
)]
pub async fn preview_handler(
    State(_state): State<AppState>,
    _user: AuthedUser,
    Json(req): Json<PreviewRequest>,
) -> Result<Json<PreviewResponse>, ApiError> {
    let schedule = parse_cron(&req.schedule_cron).map_err(ApiError::from)?;
    let tz = parse_timezone(&req.timezone).map_err(ApiError::from)?;
    let n = req.n.min(10) as usize;
    let now = Utc::now();
    let fires = next_n_fires(&schedule, tz, now, n);
    Ok(Json(PreviewResponse {
        next_fires_unix: fires.iter().map(|t| t.timestamp()).collect(),
    }))
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
            message: "only a Controller can change routines".into(),
        }),
    }
}

#[allow(dead_code)]
fn _force_chrono_into_scope(t: chrono::DateTime<Utc>) -> i64 {
    Utc.from_utc_datetime(&t.naive_utc()).timestamp()
}

pub fn routines_router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/admin/routines",
            get(list_handler).post(create_handler),
        )
        .route(
            "/api/admin/routines/{id}",
            get(get_handler).put(update_handler).delete(delete_handler),
        )
        .route("/api/admin/routines/{id}/run-now", post(run_now_handler))
        .route("/api/admin/routines/{id}/runs", get(runs_handler))
        .route("/api/admin/routines/preview", post(preview_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::{self, Body};
    use axum::http::{header, Method, Request};
    use tower::ServiceExt;

    async fn setup_controller_token(app: &axum::Router) -> String {
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

    fn create_body() -> serde_json::Value {
        serde_json::json!({
            "name": "morning-summary",
            "schedule_cron": "0 8 * * *",
            "timezone": "America/New_York",
            "prompt": "Summarise overnight emails into the control thread.",
            "target_conversation_id": null,
            "enabled": true,
        })
    }

    #[tokio::test]
    async fn empty_list_returns_zero_routines() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/routines")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["routines"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn create_then_list_then_update_then_delete() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;

        // Create
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/routines")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(create_body().to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let id = v["id"].as_str().unwrap().to_owned();
        assert!(v["next_run_at"].is_number());

        // Update
        let mut body = create_body();
        body["name"] = "morning-summary v2".into();
        let req = Request::builder()
            .method(Method::PUT)
            .uri(format!("/api/admin/routines/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // List
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/routines")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let arr = v["routines"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "morning-summary v2");

        // Delete
        let req = Request::builder()
            .method(Method::DELETE)
            .uri(format!("/api/admin/routines/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn create_rejects_invalid_cron_with_400() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let mut body = create_body();
        body["schedule_cron"] = "not a cron".into();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/routines")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn preview_returns_n_future_times() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let body = serde_json::json!({
            "schedule_cron": "0 8 * * *",
            "timezone": "UTC",
            "n": 3,
        });
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/routines/preview")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let arr = v["next_fires_unix"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        let now = chrono::Utc::now().timestamp();
        for t in arr {
            assert!(t.as_i64().unwrap() > now);
        }
    }

    #[tokio::test]
    async fn run_now_writes_a_run_history_row() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/routines")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(create_body().to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let id = v["id"].as_str().unwrap().to_owned();

        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/admin/routines/{id}/run-now"))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // v1 placeholder dispatch marks the run Skipped with an
        // explanatory error string. When runner-local lands, this
        // becomes Success/Failed.
        assert_eq!(v["status"], "Skipped");
        assert!(v["error"].is_string());

        // Run-history endpoint sees the row.
        let req = Request::builder()
            .method(Method::GET)
            .uri(format!("/api/admin/routines/{id}/runs"))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let runs = v["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
    }

    #[tokio::test]
    async fn delete_unknown_id_returns_404() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/api/admin/routines/does-not-exist")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
