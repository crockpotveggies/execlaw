//! Runner-deployment admin routes (Phase 7).
//!
//! Read + write surface for `config_runner_deployments`. Every
//! mutation logs to `config_audit` so the operator can replay who
//! changed what. Auth-gated via `AuthedUser`; today's
//! single-controller mode means any authenticated request can
//! mutate, but Phase-7 multi-controller will add role checks here.

use crate::auth_extract::AuthedUser;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Router, routing::get};
use execlaw_core::audit::AuditStore;
use execlaw_core::deployments::{
    DeploymentError, DeploymentPatch, DeploymentPurpose, DeploymentRow,
    DeploymentStore,
};
use execlaw_core::ids::DeploymentId;
use serde::{Deserialize, Serialize};

// -----------------------------------------------------------------------
// Wire types
// -----------------------------------------------------------------------

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DeploymentView {
    pub id: String,
    pub purpose: String,
    pub inference_backend: String,
    pub model_spec: serde_json::Value,
    pub gpu_id: Option<String>,
    pub endpoint: Option<String>,
    pub is_default: bool,
    pub active: bool,
    pub notes: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<DeploymentRow> for DeploymentView {
    fn from(r: DeploymentRow) -> Self {
        Self {
            id: r.id.as_str().to_owned(),
            purpose: r.purpose.as_str().to_owned(),
            inference_backend: r.inference_backend,
            model_spec: r.model_spec,
            gpu_id: r.gpu_id,
            endpoint: r.endpoint,
            is_default: r.is_default,
            active: r.active,
            notes: r.notes,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DeploymentListResponse {
    pub deployments: Vec<DeploymentView>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateDeploymentRequest {
    /// Optional client-supplied id. Server mints a UUID when omitted.
    #[serde(default)]
    pub id: Option<String>,
    pub purpose: String,
    pub inference_backend: String,
    pub model_spec: serde_json::Value,
    #[serde(default)]
    pub gpu_id: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default)]
    pub notes: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateDeploymentRequest {
    #[serde(default)]
    pub purpose: Option<String>,
    #[serde(default)]
    pub inference_backend: Option<String>,
    #[serde(default)]
    pub model_spec: Option<serde_json::Value>,
    /// Three-valued: missing = leave alone; `null` = clear; string = set.
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    #[schema(value_type = Option<String>)]
    pub gpu_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    #[schema(value_type = Option<String>)]
    pub endpoint: Option<Option<String>>,
    #[serde(default)]
    pub is_default: Option<bool>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    #[schema(value_type = Option<String>)]
    pub notes: Option<Option<String>>,
}

fn deserialize_optional_field<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Ok(Some(Option::deserialize(de)?))
}

// -----------------------------------------------------------------------
// Handlers
// -----------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/api/admin/deployments",
    responses(
        (status = 200, description = "Runner deployment registry", body = DeploymentListResponse),
        (status = 401, description = "Missing or invalid Authorization header"),
    ),
    security(("bearer_jwt" = [])),
    tag = "deployments"
)]
pub async fn list_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> impl IntoResponse {
    let store = DeploymentStore::new(&state.db);
    match store.list() {
        Ok(rows) => {
            let deployments: Vec<DeploymentView> =
                rows.into_iter().map(Into::into).collect();
            (
                StatusCode::OK,
                Json(serde_json::json!(DeploymentListResponse { deployments })),
            )
                .into_response()
        }
        Err(e) => internal(&format!("list: {e}")),
    }
}

#[utoipa::path(
    post,
    path = "/api/admin/deployments",
    request_body = CreateDeploymentRequest,
    responses(
        (status = 200, description = "Deployment created", body = DeploymentView),
        (status = 400, description = "Invalid purpose / empty backend / bad JSON"),
        (status = 401, description = "Missing or invalid Authorization header"),
    ),
    security(("bearer_jwt" = [])),
    tag = "deployments"
)]
pub async fn create_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<CreateDeploymentRequest>,
) -> impl IntoResponse {
    let purpose = match DeploymentPurpose::parse(&req.purpose) {
        Some(p) => p,
        None => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_purpose",
                &format!(
                    "purpose must be one of: Standard | Reasoning | Guardrail | VoiceSTT | VoiceTTS; got {:?}",
                    req.purpose
                ),
            );
        }
    };
    let id = req
        .id
        .unwrap_or_else(|| format!("dep-{}", uuid::Uuid::new_v4()));
    let now = chrono::Utc::now().timestamp();
    let row = DeploymentRow {
        id: DeploymentId::from(id.as_str()),
        purpose,
        inference_backend: req.inference_backend,
        model_spec: req.model_spec,
        gpu_id: req.gpu_id,
        endpoint: req.endpoint,
        is_default: req.is_default,
        active: req.active,
        notes: req.notes,
        created_at: now,
        updated_at: now,
    };
    let store = DeploymentStore::new(&state.db);
    match store.insert(&row) {
        Ok(()) => {
            // Audit-log the create. Failure of the audit write
            // shouldn't fail the route — the row is the source of
            // truth, the audit is a nice-to-have feed.
            let _ = AuditStore::new(&state.db).insert(
                &user.user_id,
                "config_runner_deployments",
                row.id.as_str(),
                None,
                Some(&serde_json::to_value(DeploymentView::from(row.clone())).unwrap_or(serde_json::Value::Null)),
            );
            (
                StatusCode::OK,
                Json(serde_json::json!(DeploymentView::from(row))),
            )
                .into_response()
        }
        Err(DeploymentError::EmptyBackend) => error(
            StatusCode::BAD_REQUEST,
            "empty_backend",
            "inference_backend must not be empty",
        ),
        Err(DeploymentError::InvalidSpec(msg)) => {
            error(StatusCode::BAD_REQUEST, "invalid_spec", &msg)
        }
        Err(e) => internal(&format!("insert: {e}")),
    }
}

#[utoipa::path(
    patch,
    path = "/api/admin/deployments/{id}",
    request_body = UpdateDeploymentRequest,
    params(
        ("id" = String, Path, description = "Deployment id"),
    ),
    responses(
        (status = 200, description = "Updated deployment row", body = DeploymentView),
        (status = 400, description = "Invalid purpose / empty backend"),
        (status = 401, description = "Missing or invalid Authorization header"),
        (status = 404, description = "Deployment not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "deployments"
)]
pub async fn update_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
    Json(req): Json<UpdateDeploymentRequest>,
) -> impl IntoResponse {
    let purpose = if let Some(s) = req.purpose.as_deref() {
        match DeploymentPurpose::parse(s) {
            Some(p) => Some(p),
            None => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "invalid_purpose",
                    &format!("purpose must be one of the well-known set; got {s:?}"),
                );
            }
        }
    } else {
        None
    };
    let patch = DeploymentPatch {
        purpose,
        inference_backend: req.inference_backend,
        model_spec: req.model_spec,
        gpu_id: req.gpu_id,
        endpoint: req.endpoint,
        is_default: req.is_default,
        active: req.active,
        notes: req.notes,
    };
    let store = DeploymentStore::new(&state.db);
    let did = DeploymentId::from(id.as_str());
    let prev = store.get(&did).ok().flatten();
    let now = chrono::Utc::now().timestamp();
    match store.update(&did, &patch, now) {
        Ok(updated) => {
            let prev_json = prev
                .and_then(|r| serde_json::to_value(DeploymentView::from(r)).ok());
            let new_json =
                serde_json::to_value(DeploymentView::from(updated.clone())).ok();
            let _ = AuditStore::new(&state.db).insert(
                &user.user_id,
                "config_runner_deployments",
                updated.id.as_str(),
                prev_json.as_ref(),
                new_json.as_ref(),
            );
            (
                StatusCode::OK,
                Json(serde_json::json!(DeploymentView::from(updated))),
            )
                .into_response()
        }
        Err(DeploymentError::NotFound(id)) => {
            error(StatusCode::NOT_FOUND, "not_found", &format!("no deployment with id {id}"))
        }
        Err(DeploymentError::EmptyBackend) => {
            error(StatusCode::BAD_REQUEST, "empty_backend", "inference_backend must not be empty")
        }
        Err(DeploymentError::InvalidSpec(msg)) => {
            error(StatusCode::BAD_REQUEST, "invalid_spec", &msg)
        }
        Err(e) => internal(&format!("update: {e}")),
    }
}

#[utoipa::path(
    delete,
    path = "/api/admin/deployments/{id}",
    params(
        ("id" = String, Path, description = "Deployment id"),
    ),
    responses(
        (status = 200, description = "Deployment deleted"),
        (status = 401, description = "Missing or invalid Authorization header"),
        (status = 404, description = "Deployment not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "deployments"
)]
pub async fn delete_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let store = DeploymentStore::new(&state.db);
    let did = DeploymentId::from(id.as_str());
    let prev = store.get(&did).ok().flatten();
    match store.delete(&did) {
        Ok(()) => {
            let prev_json = prev
                .and_then(|r| serde_json::to_value(DeploymentView::from(r)).ok());
            let _ = AuditStore::new(&state.db).insert(
                &user.user_id,
                "config_runner_deployments",
                &id,
                prev_json.as_ref(),
                None,
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({"ok": true})),
            )
                .into_response()
        }
        Err(DeploymentError::NotFound(_)) => {
            error(StatusCode::NOT_FOUND, "not_found", &format!("no deployment with id {id}"))
        }
        Err(e) => internal(&format!("delete: {e}")),
    }
}

pub fn deployments_router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/admin/deployments",
            get(list_handler).post(create_handler),
        )
        .route(
            "/api/admin/deployments/{id}",
            axum::routing::patch(update_handler).delete(delete_handler),
        )
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn error(status: StatusCode, code: &str, message: &str) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({
            "error": {"code": code, "message": message}
        })),
    )
        .into_response()
}

fn internal(msg: &str) -> axum::response::Response {
    tracing::error!("{msg}");
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal", msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::{self, Body};
    use axum::http::{Method, Request, header};
    use tower::ServiceExt;

    async fn setup_get_token(app: &axum::Router) -> String {
        let body = serde_json::to_vec(&serde_json::json!({
            "username": "tester",
            "admin_password": "hunter2-longer",
            "display_name": "Tester",
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

    async fn json_request(
        app: &axum::Router,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    fn create_body() -> serde_json::Value {
        serde_json::json!({
            "purpose": "Standard",
            "inference_backend": "service-vllm",
            "model_spec": {"model": "Qwen3.5-27B-AWQ"},
            "endpoint": "http://127.0.0.1:8000/v1",
            "is_default": true,
        })
    }

    #[tokio::test]
    async fn deployments_list_requires_auth() {
        let app = build_router(test_app_state());
        let (status, _) =
            json_request(&app, Method::GET, "/api/admin/deployments", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn deployments_full_crud_round_trip() {
        let app = build_router(test_app_state());
        let token = setup_get_token(&app).await;

        // Empty list.
        let (s, body) = json_request(
            &app,
            Method::GET,
            "/api/admin/deployments",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["deployments"].as_array().unwrap().len(), 0);

        // Create.
        let (s, body) = json_request(
            &app,
            Method::POST,
            "/api/admin/deployments",
            Some(&token),
            Some(create_body()),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["purpose"], "Standard");
        let id = body["id"].as_str().unwrap().to_owned();

        // List shows it.
        let (s, body) = json_request(
            &app,
            Method::GET,
            "/api/admin/deployments",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["deployments"].as_array().unwrap().len(), 1);
        assert_eq!(body["deployments"][0]["id"], id);

        // Update notes.
        let (s, body) = json_request(
            &app,
            Method::PATCH,
            &format!("/api/admin/deployments/{id}"),
            Some(&token),
            Some(serde_json::json!({"notes": "production primary"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["notes"], "production primary");

        // Delete.
        let (s, _) = json_request(
            &app,
            Method::DELETE,
            &format!("/api/admin/deployments/{id}"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);

        // Now empty again.
        let (_, body) = json_request(
            &app,
            Method::GET,
            "/api/admin/deployments",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(body["deployments"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn create_rejects_invalid_purpose() {
        let app = build_router(test_app_state());
        let token = setup_get_token(&app).await;
        let (s, body) = json_request(
            &app,
            Method::POST,
            "/api/admin/deployments",
            Some(&token),
            Some(serde_json::json!({
                "purpose": "Mystery",
                "inference_backend": "x",
                "model_spec": {},
            })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_purpose");
    }

    #[tokio::test]
    async fn create_rejects_empty_backend() {
        let app = build_router(test_app_state());
        let token = setup_get_token(&app).await;
        let (s, body) = json_request(
            &app,
            Method::POST,
            "/api/admin/deployments",
            Some(&token),
            Some(serde_json::json!({
                "purpose": "Standard",
                "inference_backend": "  ",
                "model_spec": {},
            })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "empty_backend");
    }

    #[tokio::test]
    async fn update_unknown_id_is_404() {
        let app = build_router(test_app_state());
        let token = setup_get_token(&app).await;
        let (s, body) = json_request(
            &app,
            Method::PATCH,
            "/api/admin/deployments/missing",
            Some(&token),
            Some(serde_json::json!({"notes": "x"})),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "not_found");
    }

    /// Mutations land in the audit log automatically.
    #[tokio::test]
    async fn create_then_audit_log_shows_actor_and_new_payload() {
        let app = build_router(test_app_state());
        let token = setup_get_token(&app).await;
        let (s, body) = json_request(
            &app,
            Method::POST,
            "/api/admin/deployments",
            Some(&token),
            Some(create_body()),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let id = body["id"].as_str().unwrap();
        let (s, audit) =
            json_request(&app, Method::GET, "/api/admin/audit", Some(&token), None).await;
        assert_eq!(s, StatusCode::OK);
        let entries = audit["entries"].as_array().unwrap();
        let entry = entries
            .iter()
            .find(|e| e["row_id"] == id)
            .expect("audit log should contain the create");
        assert_eq!(entry["table_name"], "config_runner_deployments");
        assert!(entry["actor"].as_str().unwrap().starts_with("controller-"));
        assert!(entry["new_json"].is_object());
        assert!(entry["old_json"].is_null());
    }
}
