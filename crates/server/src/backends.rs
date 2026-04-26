//! Settings → Backends — inference-backend-per-purpose admin
//! routes (Phase 8.5; reshaped in 8.8).
//!
//! There is no create or delete affordance: the set of purposes is
//! a fixed enum (Standard / Small / VoiceSTT / VoiceTTS) and the
//! operator's job is to configure which model + GPU + endpoint
//! serves each one. Standard additionally exposes a
//! `reasoning_enabled` flag that opts the runner into the model's
//! native reasoning mode for that backend. A purpose without a configured
//! backend is rendered as "not configured" by the SPA — the operator
//! fills it in via PUT.
//!
//! Routes:
//!   * `GET  /api/admin/backends` — every configured backend.
//!   * `PUT  /api/admin/backends/{purpose}` — upsert by purpose
//!     (Controller-only, audit-logged).
//!   * `DELETE /api/admin/backends/{purpose}` — clear the slot
//!     (Controller-only). Useful when the operator is wiping a
//!     misconfigured backend and wants a clean re-fill rather than
//!     PUTting a placeholder.

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, put};
use axum::Router;
use execlaw_core::audit::AuditStore;
use execlaw_core::backends::{
    BackendError, BackendMode, BackendPurpose, BackendRow, BackendStore, BackendUpsert,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct BackendView {
    pub purpose: String,
    pub inference_backend: String,
    #[schema(value_type = serde_json::Value)]
    pub model_spec: serde_json::Value,
    pub gpu_id: Option<String>,
    pub endpoint: Option<String>,
    pub notes: Option<String>,
    /// Phase-8.8: whether reasoning mode is engaged on this
    /// backend. Only meaningful for Standard; the SPA hides the
    /// checkbox for the other purposes.
    pub reasoning_enabled: bool,
    /// True when this purpose accepts a reasoning_enabled value.
    /// Lets the SPA decide whether to render the checkbox without
    /// duplicating the enum locally.
    pub supports_reasoning_toggle: bool,
    /// Phase 12 — lifecycle ownership. One of "external" |
    /// "managed". Defaults to external for every existing row; the
    /// SPA exposes a Mode toggle.
    pub mode: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<&BackendRow> for BackendView {
    fn from(r: &BackendRow) -> Self {
        Self {
            purpose: r.purpose.as_str().to_owned(),
            inference_backend: r.inference_backend.clone(),
            model_spec: r.model_spec_json.clone(),
            gpu_id: r.gpu_id.clone(),
            endpoint: r.endpoint.clone(),
            notes: r.notes.clone(),
            reasoning_enabled: r.reasoning_enabled,
            supports_reasoning_toggle: r.purpose.supports_reasoning_toggle(),
            mode: r.mode.as_str().to_owned(),
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackendListResponse {
    /// One entry per `BackendPurpose::all()` value. Purposes the
    /// operator hasn't configured yet are present with `configured: false`
    /// and the rest of the fields elided so the SPA can render the
    /// fixed-row layout without per-purpose fallbacks.
    pub backends: Vec<BackendListEntry>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackendListEntry {
    pub purpose: String,
    pub configured: bool,
    pub backend: Option<BackendView>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpsertBackendRequest {
    pub inference_backend: String,
    /// Free-form JSON whose schema is plugin-defined. Validated only
    /// for parseability.
    #[schema(value_type = serde_json::Value)]
    pub model_spec: serde_json::Value,
    #[serde(default)]
    pub gpu_id: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Phase-8.8: optional reasoning toggle. Server zeroes it for
    /// purposes that don't support reasoning, so the SPA can pass
    /// it freely without per-purpose branching.
    #[serde(default)]
    pub reasoning_enabled: bool,
    /// Phase 12 — lifecycle ownership. Defaults to "external" so
    /// pre-Phase-12 SPAs that don't send the field keep their
    /// current behaviour. Unknown values are rejected with 400.
    #[serde(default = "default_mode")]
    pub mode: String,
}

fn default_mode() -> String {
    "external".to_owned()
}

impl From<BackendError> for ApiError {
    fn from(err: BackendError) -> Self {
        match err {
            BackendError::Db(e) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "db_error",
                message: e.to_string(),
            },
            BackendError::Sqlite(e) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "db_error",
                message: e.to_string(),
            },
            BackendError::Invalid(msg) => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_backend",
                message: msg,
            },
            BackendError::NotFound(p) => ApiError {
                status: StatusCode::NOT_FOUND,
                code: "backend_not_configured",
                message: format!("no backend configured for purpose '{p}'"),
            },
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/backends",
    responses((status = 200, description = "Inference backends per purpose", body = BackendListResponse)),
    security(("bearer_jwt" = [])),
    tag = "backends"
)]
pub async fn list_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<BackendListResponse>, ApiError> {
    let store = BackendStore::new(&state.db);
    let configured = store.list_all().map_err(ApiError::from)?;
    // Build a fixed-length response — one entry per BackendPurpose
    // — so the SPA renders the same shape regardless of how many
    // are configured.
    let entries: Vec<BackendListEntry> = BackendPurpose::all()
        .iter()
        .map(|p| {
            let row = configured.iter().find(|r| r.purpose == *p);
            BackendListEntry {
                purpose: p.as_str().to_owned(),
                configured: row.is_some(),
                backend: row.map(BackendView::from),
            }
        })
        .collect();
    Ok(Json(BackendListResponse { backends: entries }))
}

#[utoipa::path(
    put,
    path = "/api/admin/backends/{purpose}",
    request_body = UpsertBackendRequest,
    params(("purpose" = String, Path, description = "Backend purpose: Standard | Small | VoiceSTT | VoiceTTS")),
    responses(
        (status = 200, description = "Upserted", body = BackendView),
        (status = 400, description = "Unknown purpose / malformed model_spec"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "backends"
)]
pub async fn upsert_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(purpose): AxumPath<String>,
    Json(req): Json<UpsertBackendRequest>,
) -> Result<Json<BackendView>, ApiError> {
    require_controller(&state, &user)?;
    let purpose = BackendPurpose::parse(&purpose).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_purpose",
        message: format!("'{purpose}' is not a recognised backend purpose"),
    })?;
    if req.inference_backend.trim().is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "missing_backend",
            message: "inference_backend is required".into(),
        });
    }
    let mode = BackendMode::parse(&req.mode).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_mode",
        message: format!(
            "'{}' is not a recognised backend mode (expected 'external' or 'managed')",
            req.mode
        ),
    })?;
    let store = BackendStore::new(&state.db);
    let prior = store.get(purpose).map_err(ApiError::from)?;
    let now = chrono::Utc::now().timestamp();
    let row = store
        .upsert(
            &BackendUpsert {
                purpose,
                inference_backend: req.inference_backend.clone(),
                model_spec_json: req.model_spec.clone(),
                gpu_id: req.gpu_id.clone(),
                endpoint: req.endpoint.clone(),
                notes: req.notes.clone(),
                reasoning_enabled: req.reasoning_enabled,
                mode,
            },
            now,
        )
        .map_err(ApiError::from)?;

    // Phase 12 closure — kick the supervisor so the reconcile
    // happens within milliseconds instead of waiting up to one
    // tick (~5s). Also reset the per-purpose restart counter so a
    // row that was parked CrashLooping gets a fresh runway after
    // the operator edits its spec. Both are no-ops for external
    // rows; the supervisor only acts on Managed.
    if let Some(sup) = state.backend_supervisor.as_ref() {
        sup.reset_attempts(purpose).await;
        sup.kick();
    }

    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "config_backends",
        purpose.as_str(),
        prior.as_ref().map(|r| {
            serde_json::json!({
                "inference_backend": r.inference_backend,
                "endpoint": r.endpoint,
                "gpu_id": r.gpu_id,
            })
        }).as_ref(),
        Some(&serde_json::json!({
            "inference_backend": row.inference_backend,
            "endpoint": row.endpoint,
            "gpu_id": row.gpu_id,
        })),
    );

    Ok(Json((&row).into()))
}

#[utoipa::path(
    delete,
    path = "/api/admin/backends/{purpose}",
    params(("purpose" = String, Path, description = "Backend purpose to clear")),
    responses(
        (status = 200, description = "Slot cleared"),
        (status = 400, description = "Unknown purpose"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "Slot was already empty"),
    ),
    security(("bearer_jwt" = [])),
    tag = "backends"
)]
pub async fn clear_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(purpose): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let purpose = BackendPurpose::parse(&purpose).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_purpose",
        message: format!("'{purpose}' is not a recognised backend purpose"),
    })?;
    let store = BackendStore::new(&state.db);
    let prior = store.get(purpose).map_err(ApiError::from)?;
    let removed = store.clear(purpose).map_err(ApiError::from)?;
    if !removed {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "backend_not_configured",
            message: format!(
                "purpose '{}' has no backend to clear",
                purpose.as_str()
            ),
        });
    }
    if let Some(prior) = prior {
        let _ = AuditStore::new(&state.db).insert(
            &user.user_id,
            "config_backends",
            purpose.as_str(),
            Some(&serde_json::json!({
                "inference_backend": prior.inference_backend,
            })),
            None,
        );
    }
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
            message: "only a Controller can change backend configuration".into(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Phase 12.C — managed-backend status + restart routes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct BackendStatusResponse {
    pub purpose: String,
    pub mode: String,
    /// One of "Pulling" | "Starting" | "Healthy" | "CrashLooping" |
    /// "Stopped" | "NotFound". `Stopped` for external rows.
    pub status: String,
    pub endpoint: Option<String>,
    pub restart_attempts: u32,
    /// True when the supervisor is wired (Docker reachable). False
    /// in dev/test builds; the SPA shows a "Docker unreachable"
    /// notice when this is false and the row is managed.
    pub supervisor_available: bool,
}

#[utoipa::path(
    get,
    path = "/api/admin/backends/{purpose}/status",
    params(("purpose" = String, Path, description = "Backend purpose")),
    responses(
        (status = 200, description = "Live runtime status", body = BackendStatusResponse),
        (status = 400, description = "Unknown purpose"),
    ),
    security(("bearer_jwt" = [])),
    tag = "backends"
)]
pub async fn status_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
    AxumPath(purpose): AxumPath<String>,
) -> Result<Json<BackendStatusResponse>, ApiError> {
    let purpose = BackendPurpose::parse(&purpose).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_purpose",
        message: format!("'{purpose}' is not a recognised backend purpose"),
    })?;

    let store = BackendStore::new(&state.db);
    let row = store.get(purpose).map_err(ApiError::from)?;
    // No row at all → Stopped + external. The supervisor never sees
    // a row that doesn't exist; the SPA still wants a response so
    // the status pill renders deterministically.
    let (mode, endpoint) = match row {
        Some(r) => (r.mode, r.endpoint),
        None => (BackendMode::External, None),
    };

    let supervisor_available = state.backend_supervisor.is_some();
    let (status, restart_attempts) = if let Some(sup) = state.backend_supervisor.as_ref() {
        let snap = sup.snapshot_status().await;
        let entry = snap.iter().find(|s| s.purpose == purpose);
        match entry {
            Some(e) => (
                runtime_status_str(&e.status).to_owned(),
                e.restart_attempts,
            ),
            None => ("Stopped".to_owned(), 0),
        }
    } else {
        ("Stopped".to_owned(), 0)
    };

    Ok(Json(BackendStatusResponse {
        purpose: purpose.as_str().to_owned(),
        mode: mode.as_str().to_owned(),
        status,
        endpoint,
        restart_attempts,
        supervisor_available,
    }))
}

#[utoipa::path(
    post,
    path = "/api/admin/backends/{purpose}/restart",
    params(("purpose" = String, Path, description = "Backend purpose")),
    responses(
        (status = 200, description = "Restart queued"),
        (status = 400, description = "Unknown purpose"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 503, description = "Supervisor not running (Docker unreachable)"),
    ),
    security(("bearer_jwt" = [])),
    tag = "backends"
)]
pub async fn restart_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(purpose): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let purpose = BackendPurpose::parse(&purpose).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_purpose",
        message: format!("'{purpose}' is not a recognised backend purpose"),
    })?;
    let sup = state
        .backend_supervisor
        .as_ref()
        .ok_or_else(|| ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "supervisor_unavailable",
            message: "backend supervisor is not running (Docker unreachable?)".into(),
        })?;
    sup.restart(purpose).await.map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "supervisor_error",
        message: e.to_string(),
    })?;
    Ok(StatusCode::OK)
}

fn runtime_status_str(s: &execlaw_container_manager::ServiceStatus) -> &'static str {
    use execlaw_container_manager::ServiceStatus::*;
    match s {
        Pulling => "Pulling",
        Starting => "Starting",
        Healthy => "Healthy",
        CrashLooping { .. } => "CrashLooping",
        Stopped => "Stopped",
        NotFound => "NotFound",
    }
}

pub fn backends_router() -> Router<AppState> {
    use axum::routing::post;
    Router::new()
        .route("/api/admin/backends", get(list_handler))
        .route(
            "/api/admin/backends/{purpose}",
            put(upsert_handler).delete(clear_handler),
        )
        .route("/api/admin/backends/{purpose}/status", get(status_handler))
        .route("/api/admin/backends/{purpose}/restart", post(restart_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::{self, Body};
    use axum::http::{Method, Request, header};
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

    fn upsert_body() -> serde_json::Value {
        serde_json::json!({
            "inference_backend": "service-vllm",
            "model_spec": {"model": "Qwen3.5-27B-AWQ"},
            "gpu_id": "0",
            "endpoint": "http://127.0.0.1:8000/v1",
            "notes": null
        })
    }

    #[tokio::test]
    async fn list_returns_one_entry_per_purpose_even_when_unconfigured() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/backends")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let arr = v["backends"].as_array().unwrap();
        // Always four purposes regardless of how many are filled in.
        assert_eq!(arr.len(), 4);
        for entry in arr {
            assert_eq!(entry["configured"], false);
            assert!(entry["backend"].is_null());
        }
    }

    #[tokio::test]
    async fn upsert_creates_then_updates_in_place() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Standard")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(upsert_body().to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Second PUT updates the same row.
        let mut body2 = upsert_body();
        body2["endpoint"] = serde_json::Value::String("http://127.0.0.1:9000/v1".into());
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Standard")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body2.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["endpoint"], "http://127.0.0.1:9000/v1");

        // List now shows configured = true for Standard.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/backends")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let standard = v["backends"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["purpose"] == "Standard")
            .unwrap();
        assert_eq!(standard["configured"], true);
    }

    #[tokio::test]
    async fn upsert_rejects_unknown_purpose() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Hallucinated")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(upsert_body().to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn clear_returns_404_for_unconfigured_slot() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/api/admin/backends/Small")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn clear_round_trip() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        // Upsert then clear.
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/admin/backends/Standard")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(upsert_body().to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/api/admin/backends/Standard")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn list_returns_default_external_mode_for_existing_rows() {
        // Phase 12.A: a backend created without a `mode` field
        // (legacy SPA, missing field) defaults to external. The
        // BackendView in the response carries that string back so
        // the SPA can render the toggle.
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/admin/backends/Standard")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(upsert_body().to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/admin/backends")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let standard = v["backends"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["purpose"] == "Standard")
            .unwrap();
        assert_eq!(
            standard["backend"]["mode"], "external",
            "absent mode field defaults to external on the wire"
        );
    }

    #[tokio::test]
    async fn upsert_managed_round_trips_mode() {
        // Phase 12.A: an explicit `mode: "managed"` round-trips on
        // the View. The endpoint can be null at this stage — the
        // BackendSupervisor (Phase 12.C) writes it back after spawn.
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let mut body = upsert_body();
        body["mode"] = serde_json::Value::String("managed".into());
        body["endpoint"] = serde_json::Value::Null;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Standard")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["mode"], "managed");
        assert!(v["endpoint"].is_null());
    }

    #[tokio::test]
    async fn upsert_rejects_unknown_mode() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let mut body = upsert_body();
        body["mode"] = serde_json::Value::String("kubernetes".into());
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Standard")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn status_route_reports_supervisor_unavailable_in_test_state() {
        // Phase 12.C — test_app_state ships supervisor=None, so the
        // status route must surface that fact (so the SPA can show a
        // "Docker unreachable" notice).
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/backends/Standard/status")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["mode"], "external");
        assert_eq!(v["status"], "Stopped");
        assert_eq!(v["supervisor_available"], false);
    }

    #[tokio::test]
    async fn status_route_with_managed_supervisor_reports_healthy() {
        // Build an AppState with a mock-controller-backed supervisor
        // and a managed Standard row. After two reconcile passes the
        // supervisor reports Healthy and the status route surfaces
        // it.
        use execlaw_container_manager::MockServiceController;
        use execlaw_core::backends::BackendUpsert;
        let mut state = test_app_state();
        let mock = std::sync::Arc::new(MockServiceController::new());
        let sup = crate::backend_supervisor::BackendSupervisor::new(
            state.db.clone(),
            mock.clone(),
        );
        state.backend_supervisor = Some(sup.clone());

        // Seed a managed Standard row.
        execlaw_core::backends::BackendStore::new(&state.db)
            .upsert(
                &BackendUpsert {
                    purpose: BackendPurpose::Standard,
                    inference_backend: "service-vllm".into(),
                    model_spec_json: serde_json::json!({
                        "image": "vllm:test",
                        "args": ["--model", "X"],
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
            .unwrap();
        sup.reconcile_once().await;
        sup.reconcile_once().await;

        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/backends/Standard/status")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["mode"], "managed");
        assert_eq!(v["status"], "Healthy");
        assert_eq!(v["supervisor_available"], true);
        assert!(v["endpoint"].as_str().unwrap().starts_with("http://127.0.0.1:"));
    }

    #[tokio::test]
    async fn restart_route_503_when_supervisor_absent() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/backends/Standard/restart")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn reasoning_flag_is_zeroed_for_non_standard_purposes() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        // Send reasoning_enabled = true on a Small backend; server
        // must zero it because Small doesn't expose the toggle.
        let mut body = upsert_body();
        body["reasoning_enabled"] = serde_json::Value::Bool(true);
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Small")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["reasoning_enabled"], false);
        assert_eq!(v["supports_reasoning_toggle"], false);

        // Standard with reasoning_enabled = true round-trips truthy.
        let mut body2 = upsert_body();
        body2["reasoning_enabled"] = serde_json::Value::Bool(true);
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Standard")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body2.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["reasoning_enabled"], true);
        assert_eq!(v["supports_reasoning_toggle"], true);
    }
}
