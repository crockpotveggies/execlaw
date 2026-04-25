//! Settings → MCP servers (Phase 8c admin routes).
//!
//! CRUD over `config_mcp_servers`. Controller-only; every mutation
//! goes through `AuditStore`. After a write the connection manager
//! is asked to `reconcile()` so the operator's change takes effect
//! without a process restart.

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use execlaw_core::audit::AuditStore;
use execlaw_core::mcp_servers::{
    validate_id, McpServerInsert, McpServerRow, McpServerStore, McpTransport,
};
use execlaw_policy::trust::TrustLevel;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;

/// JSON shape for the SPA. Mirrors `McpServerRow` but renders enums
/// as strings.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct McpServerView {
    pub id: String,
    pub display_name: String,
    pub transport: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub auth_secret_ref: Option<String>,
    pub enabled: bool,
    pub default_allowed_classes: Vec<String>,
    pub status: String,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<&McpServerRow> for McpServerView {
    fn from(r: &McpServerRow) -> Self {
        Self {
            id: r.id.clone(),
            display_name: r.display_name.clone(),
            transport: r.transport.as_str().into(),
            command: r.command.clone(),
            args: r.args.clone(),
            env: r.env.clone(),
            cwd: r.cwd.clone(),
            url: r.url.clone(),
            auth_secret_ref: r.auth_secret_ref.clone(),
            enabled: r.enabled,
            default_allowed_classes: r.default_allowed_classes.clone(),
            status: r.status.as_str().into(),
            last_error: r.last_error.clone(),
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct McpServerListResponse {
    pub servers: Vec<McpServerView>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct McpServerWriteRequest {
    pub id: String,
    pub display_name: String,
    /// `"stdio"` is currently the only fully-supported transport.
    /// `"streamable_http"` is accepted for forward-compat but the
    /// connection manager warns + skips on connect.
    pub transport: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub auth_secret_ref: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_classes")]
    pub default_allowed_classes: Vec<String>,
}

fn default_enabled() -> bool {
    true
}
fn default_classes() -> Vec<String> {
    vec!["Controller".into()]
}

#[utoipa::path(
    get,
    path = "/api/admin/mcp/servers",
    responses((status = 200, description = "Configured MCP servers", body = McpServerListResponse)),
    security(("bearer_jwt" = [])),
    tag = "mcp"
)]
pub async fn list_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<McpServerListResponse>, ApiError> {
    let rows = McpServerStore::new(&state.db)
        .list_all()
        .map_err(ApiError::from)?;
    Ok(Json(McpServerListResponse {
        servers: rows.iter().map(McpServerView::from).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/admin/mcp/servers",
    request_body = McpServerWriteRequest,
    responses(
        (status = 201, description = "Server created", body = McpServerView),
        (status = 400, description = "Invalid id / transport / classes"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 409, description = "Server with this id already exists"),
    ),
    security(("bearer_jwt" = [])),
    tag = "mcp"
)]
pub async fn create_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<McpServerWriteRequest>,
) -> Result<(StatusCode, Json<McpServerView>), ApiError> {
    require_controller(&state, &user)?;
    let insert = build_insert(&req)?;
    let store = McpServerStore::new(&state.db);
    if store.get(&insert.id).map_err(ApiError::from)?.is_some() {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "mcp_server_exists",
            message: format!("MCP server '{}' already exists", insert.id),
        });
    }
    let now = chrono::Utc::now().timestamp();
    store.insert(&insert, now).map_err(ApiError::from)?;
    let row = store
        .get(&insert.id)
        .map_err(ApiError::from)?
        .ok_or_else(|| internal("row vanished after insert"))?;
    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "config_mcp_servers",
        &row.id,
        None,
        Some(&serde_json::json!({
            "display_name": row.display_name,
            "transport": row.transport.as_str(),
            "enabled": row.enabled,
        })),
    );
    state.mcp_host.reconcile().await;
    Ok((StatusCode::CREATED, Json((&row).into())))
}

#[utoipa::path(
    post,
    path = "/api/admin/mcp/servers/{id}",
    request_body = McpServerWriteRequest,
    params(("id" = String, Path, description = "MCP server id")),
    responses(
        (status = 200, description = "Server updated", body = McpServerView),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "Server not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "mcp"
)]
pub async fn update_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<McpServerWriteRequest>,
) -> Result<Json<McpServerView>, ApiError> {
    require_controller(&state, &user)?;
    if req.id != id {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "id_mismatch",
            message: "request body id must match path id".into(),
        });
    }
    let insert = build_insert(&req)?;
    let store = McpServerStore::new(&state.db);
    let prior = store
        .get(&id)
        .map_err(ApiError::from)?
        .ok_or_else(|| not_found(&id))?;
    let now = chrono::Utc::now().timestamp();
    let updated = store
        .update(&id, &insert, now)
        .map_err(ApiError::from)?;
    if !updated {
        return Err(not_found(&id));
    }
    let after = store
        .get(&id)
        .map_err(ApiError::from)?
        .ok_or_else(|| internal("row vanished after update"))?;
    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "config_mcp_servers",
        &id,
        Some(&serde_json::json!({"enabled": prior.enabled, "transport": prior.transport.as_str()})),
        Some(&serde_json::json!({"enabled": after.enabled, "transport": after.transport.as_str()})),
    );
    state.mcp_host.reconcile().await;
    Ok(Json((&after).into()))
}

#[utoipa::path(
    post,
    path = "/api/admin/mcp/servers/{id}/delete",
    params(("id" = String, Path, description = "MCP server id")),
    responses(
        (status = 200, description = "Server removed"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "Server not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "mcp"
)]
pub async fn delete_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let store = McpServerStore::new(&state.db);
    let prior = store
        .get(&id)
        .map_err(ApiError::from)?
        .ok_or_else(|| not_found(&id))?;
    let removed = store.delete(&id).map_err(ApiError::from)?;
    if !removed {
        return Err(not_found(&id));
    }
    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "config_mcp_servers",
        &id,
        Some(&serde_json::json!({"display_name": prior.display_name})),
        None,
    );
    state.mcp_host.reconcile().await;
    Ok(StatusCode::OK)
}

fn build_insert(req: &McpServerWriteRequest) -> Result<McpServerInsert, ApiError> {
    validate_id(&req.id).map_err(|e| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_id",
        message: e.into(),
    })?;
    let transport = McpTransport::parse(&req.transport).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_transport",
        message: format!("transport must be 'stdio' or 'streamable_http' (got '{}')", req.transport),
    })?;
    if transport == McpTransport::Stdio && req.command.is_none() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "stdio_command_required",
            message: "stdio MCP servers require a 'command' field".into(),
        });
    }
    if transport == McpTransport::StreamableHttp && req.url.is_none() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "http_url_required",
            message: "streamable_http MCP servers require a 'url' field".into(),
        });
    }
    for cls in &req.default_allowed_classes {
        if TrustLevel::parse(cls).is_none() {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "unknown_trust_class",
                message: format!("'{cls}' is not a valid trust class"),
            });
        }
    }
    Ok(McpServerInsert {
        id: req.id.clone(),
        display_name: req.display_name.clone(),
        transport,
        command: req.command.clone(),
        args: req.args.clone(),
        env: req.env.clone(),
        cwd: req.cwd.clone(),
        url: req.url.clone(),
        auth_secret_ref: req.auth_secret_ref.clone(),
        enabled: req.enabled,
        default_allowed_classes: req.default_allowed_classes.clone(),
    })
}

fn require_controller(state: &AppState, user: &AuthedUser) -> Result<(), ApiError> {
    use execlaw_core::users::{UserRole, UserStore};
    let row = UserStore::new(&state.db)
        .get_by_id(&user.user_id)
        .map_err(ApiError::from)?;
    match row.map(|u| u.role) {
        Some(UserRole::Controller) => Ok(()),
        _ => Err(ApiError {
            status: StatusCode::FORBIDDEN,
            code: "controller_only",
            message: "only a Controller can manage MCP servers".into(),
        }),
    }
}

fn not_found(id: &str) -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        code: "mcp_server_not_found",
        message: format!("no MCP server with id '{id}'"),
    }
}

fn internal(msg: &str) -> ApiError {
    ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "mcp_internal",
        message: msg.into(),
    }
}

pub fn mcp_admin_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/mcp/servers", get(list_handler).post(create_handler))
        .route("/api/admin/mcp/servers/{id}", post(update_handler))
        .route("/api/admin/mcp/servers/{id}/delete", post(delete_handler))
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

    fn write_request(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "display_name": "Mock",
            "transport": "stdio",
            "command": "/usr/bin/mock-mcp",
            "args": ["--port", "0"],
            "env": {},
            "enabled": true,
            "default_allowed_classes": ["Controller", "KnownTrusted"]
        })
    }

    #[tokio::test]
    async fn create_persists_and_list_returns_it() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/mcp/servers")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(write_request("github").to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/mcp/servers")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let servers = v["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["id"], "github");
        assert_eq!(servers[0]["transport"], "stdio");
        // Status starts at "idle" — we never spin up the actor in tests.
        assert_eq!(servers[0]["status"], "idle");
    }

    #[tokio::test]
    async fn create_rejects_invalid_id() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/mcp/servers")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(write_request("has space").to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_rejects_unknown_trust_class() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let mut body = write_request("ok-id");
        body["default_allowed_classes"] = serde_json::json!(["Hacker"]);
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/mcp/servers")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_rejects_stdio_without_command() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let mut body = write_request("ok-id");
        body["command"] = serde_json::Value::Null;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/mcp/servers")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], "stdio_command_required");
    }

    #[tokio::test]
    async fn duplicate_id_is_409_conflict() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        for _ in 0..2 {
            let req = Request::builder()
                .method(Method::POST)
                .uri("/api/admin/mcp/servers")
                .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(write_request("dup").to_string()))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            // First → 201, second → 409.
            let _ = resp;
        }
        // Repeat the second request explicitly so we can check the
        // status of the conflicting attempt.
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/mcp/servers")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(write_request("dup").to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn delete_round_trip() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/mcp/servers")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(write_request("svc").to_string()))
            .unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/mcp/servers/svc/delete")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Second delete is 404.
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/mcp/servers/svc/delete")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
