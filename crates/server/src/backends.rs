//! Settings → Backends — inference-backend-per-purpose admin
//! routes (Phase 8.5; replaces the legacy `deployments.rs` module).
//!
//! There is no create or delete affordance: the set of purposes is
//! a fixed enum (Standard / Reasoning / Guardrail / VoiceSTT /
//! VoiceTTS), and the operator's job is to configure which model +
//! GPU + endpoint serves each one. A purpose without a configured
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
use execlaw_core::backends::{BackendError, BackendPurpose, BackendRow, BackendStore, BackendUpsert};
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
    params(("purpose" = String, Path, description = "Backend purpose: Standard | Reasoning | Guardrail | VoiceSTT | VoiceTTS")),
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
            },
            now,
        )
        .map_err(ApiError::from)?;

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

pub fn backends_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/backends", get(list_handler))
        .route(
            "/api/admin/backends/{purpose}",
            put(upsert_handler).delete(clear_handler),
        )
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
        // Always five purposes regardless of how many are filled in.
        assert_eq!(arr.len(), 5);
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
            .uri("/api/admin/backends/Reasoning")
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
}
