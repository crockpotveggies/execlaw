//! Admin HTTP surface for Settings → Tools (Phase 8a).
//!
//! Two routes:
//!   * `GET  /api/admin/tools` — list every row in `config_tool_access`.
//!   * `PATCH /api/admin/tools/{tool_name}` — update enabled / allowed_classes.
//!
//! Controller-only (mirrors the deployments + users surfaces). Every
//! mutation goes through `AuditStore` so an operator change is
//! visible in the audit log.

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, patch};
use axum::Router;
use execlaw_core::tool_access::{ToolAccessRow, ToolAccessStore};
use execlaw_policy::trust::TrustLevel;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// JSON shape of a tool row in the Settings UI. Same fields as the
/// core `ToolAccessRow` but `source` is rendered as a string for the
/// SPA's discriminator.
#[derive(Debug, Serialize, ToSchema)]
pub struct ToolView {
    pub tool_name: String,
    pub source: String,
    pub source_id: Option<String>,
    pub enabled: bool,
    pub allowed_classes: Vec<String>,
    pub description: Option<String>,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub removed_at: Option<i64>,
}

impl From<&ToolAccessRow> for ToolView {
    fn from(row: &ToolAccessRow) -> Self {
        Self {
            tool_name: row.tool_name.clone(),
            source: row.source.as_str().to_owned(),
            source_id: row.source_id.clone(),
            enabled: row.enabled,
            allowed_classes: row.allowed_classes.clone(),
            description: row.description.clone(),
            first_seen_at: row.first_seen_at,
            last_seen_at: row.last_seen_at,
            removed_at: row.removed_at,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ToolListResponse {
    pub tools: Vec<ToolView>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateToolPolicyRequest {
    pub enabled: bool,
    /// Trust-class allowlist. Strings must be exact `TrustLevel`
    /// names — `Controller`, `Delegated`, `KnownTrusted`,
    /// `KnownLimited`, `UnknownPending`, `Blocked`. Unknown strings
    /// are rejected with 400 so a typo can't lock the operator out.
    pub allowed_classes: Vec<String>,
}

/// Cap on how many distinct classes can appear in one allowlist —
/// there are only six valid `TrustLevel` values so anything larger
/// is a malformed request.
const MAX_CLASSES: usize = 8;

#[utoipa::path(
    get,
    path = "/api/admin/tools",
    responses((status = 200, description = "Every registered tool", body = ToolListResponse)),
    security(("bearer_jwt" = [])),
    tag = "tools"
)]
pub async fn list_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<ToolListResponse>, ApiError> {
    let rows = ToolAccessStore::new(&state.db)
        .list_all()
        .map_err(ApiError::from)?;
    Ok(Json(ToolListResponse {
        tools: rows.iter().map(ToolView::from).collect(),
    }))
}

#[utoipa::path(
    patch,
    path = "/api/admin/tools/{tool_name}",
    request_body = UpdateToolPolicyRequest,
    params(("tool_name" = String, Path, description = "Canonical tool name")),
    responses(
        (status = 200, description = "Updated", body = ToolView),
        (status = 400, description = "Unknown trust-class string"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "Tool not registered"),
    ),
    security(("bearer_jwt" = [])),
    tag = "tools"
)]
pub async fn update_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(tool_name): AxumPath<String>,
    Json(req): Json<UpdateToolPolicyRequest>,
) -> Result<Json<ToolView>, ApiError> {
    if !is_controller(&state, &user)? {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            code: "controller_only",
            message: "only a Controller can change tool access policy".into(),
        });
    }
    if req.allowed_classes.len() > MAX_CLASSES {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "too_many_classes",
            message: format!("allowed_classes capped at {MAX_CLASSES} entries"),
        });
    }
    for cls in &req.allowed_classes {
        if TrustLevel::parse(cls).is_none() {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "unknown_trust_class",
                message: format!("'{cls}' is not a valid trust class"),
            });
        }
    }

    let store = ToolAccessStore::new(&state.db);
    let prior = store.get(&tool_name).map_err(ApiError::from)?.ok_or_else(|| ApiError {
        status: StatusCode::NOT_FOUND,
        code: "tool_not_found",
        message: format!("no tool registered as '{tool_name}'"),
    })?;
    let updated = store
        .set_policy(&tool_name, req.enabled, &req.allowed_classes)
        .map_err(ApiError::from)?;
    if !updated {
        // Lost a race with a deletion — surface as 404 so the SPA
        // re-fetches the list.
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "tool_not_found",
            message: format!("'{tool_name}' was removed mid-update"),
        });
    }
    let after = store.get(&tool_name).map_err(ApiError::from)?.ok_or_else(|| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "tool_lookup_after_update",
        message: "tool disappeared after update".into(),
    })?;

    let audit = execlaw_core::audit::AuditStore::new(&state.db);
    let _ = audit.insert(
        &user.user_id,
        "config_tool_access",
        &tool_name,
        Some(&serde_json::json!({
            "enabled": prior.enabled,
            "allowed_classes": prior.allowed_classes,
        })),
        Some(&serde_json::json!({
            "enabled": after.enabled,
            "allowed_classes": after.allowed_classes,
        })),
    );

    Ok(Json((&after).into()))
}

fn is_controller(state: &AppState, user: &AuthedUser) -> Result<bool, ApiError> {
    use execlaw_core::users::{UserRole, UserStore};
    let row = UserStore::new(&state.db)
        .get_by_id(&user.user_id)
        .map_err(ApiError::from)?;
    Ok(matches!(row.map(|u| u.role), Some(UserRole::Controller)))
}

pub fn tools_admin_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/tools", get(list_handler))
        .route("/api/admin/tools/{tool_name}", patch(update_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::{self, Body};
    use axum::http::{Method, Request, header};
    use execlaw_core::tool_access::{ToolAccessSeed, ToolAccessStore, ToolSource};
    use tower::ServiceExt;

    async fn setup_controller_token(app: &axum::Router) -> String {
        let body = serde_json::to_vec(&serde_json::json!({
            "username": "ctrl",
            "admin_password": "hunter2-longer",
            "display_name": "Controller",
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

    fn seed(store: &ToolAccessStore<'_>, name: &str) {
        store
            .upsert_seen(
                &ToolAccessSeed {
                    tool_name: name.into(),
                    source: ToolSource::Builtin,
                    source_id: None,
                    description: Some(format!("desc-{name}")),
                    input_schema: None,
                    default_allowed_classes: vec!["Controller".into(), "KnownTrusted".into()],
                },
                100,
            )
            .unwrap();
    }

    #[tokio::test]
    async fn list_tools_returns_seeded_rows_when_authenticated() {
        let state = test_app_state();
        let store = ToolAccessStore::new(&state.db);
        seed(&store, "read_memory");
        seed(&store, "set_thread_name");
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/tools")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let tools = v["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["tool_name"] == "read_memory"));
        assert!(tools.iter().any(|t| t["tool_name"] == "set_thread_name"));
    }

    #[tokio::test]
    async fn list_tools_requires_auth() {
        let app = build_router(test_app_state());
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/tools")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn update_rejects_unknown_trust_class() {
        let state = test_app_state();
        let store = ToolAccessStore::new(&state.db);
        seed(&store, "read_memory");
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let body = serde_json::json!({
            "enabled": true,
            "allowed_classes": ["Controller", "Hacker"],
        });
        let req = Request::builder()
            .method(Method::PATCH)
            .uri("/api/admin/tools/read_memory")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], "unknown_trust_class");
    }

    #[tokio::test]
    async fn update_returns_404_for_unregistered_tool() {
        let state = test_app_state();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let body = serde_json::json!({"enabled": true, "allowed_classes": ["Controller"]});
        let req = Request::builder()
            .method(Method::PATCH)
            .uri("/api/admin/tools/never_seen_tool")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_persists_new_policy_for_controller_caller() {
        let state = test_app_state();
        let store = ToolAccessStore::new(&state.db);
        seed(&store, "read_memory");
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;
        let body = serde_json::json!({
            "enabled": false,
            "allowed_classes": ["Controller"],
        });
        let req = Request::builder()
            .method(Method::PATCH)
            .uri("/api/admin/tools/read_memory")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let row = ToolAccessStore::new(&state.db)
            .get("read_memory")
            .unwrap()
            .unwrap();
        assert!(!row.enabled);
        assert_eq!(row.allowed_classes, vec!["Controller"]);
    }
}
