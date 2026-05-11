//! Admin endpoints for per-plugin settings stored in the vault.
//!
//! Each plugin owns a flat (key → value) namespace in the vault — the
//! same store that `vault_put` writes through from inside a Rhai
//! script. This module exposes two routes, both Controller-only:
//!
//!   * `GET /api/admin/plugins/{plugin_id}/settings/{key}`
//!     → returns `{ key, value }` (or 404 when unset).
//!   * `PUT /api/admin/plugins/{plugin_id}/settings/{key}`
//!     → upserts `{ value }`. The value is treated as opaque text;
//!     plugins decide how to interpret it (JSON, free-form string,
//!     etc).
//!
//! The shape is deliberately minimal — operators flip toggles and
//! configure non-secret strings here, NOT OAuth credentials (those
//! have their own admin surface). For example, `google-apps` reads
//! `enabled_modules` (a JSON string of an array) through this
//! endpoint to gate per-module dispatch.
//!
//! Trust contract: Controller-only (`require_controller`). The
//! handler shape mirrors `oauth_admin.rs` so the role check is the
//! same — a Contact-tier principal cannot reach these endpoints,
//! and a non-Controller authenticated user gets a 403.

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, put};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct PluginSettingView {
    pub plugin_id: String,
    pub key: String,
    pub value: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpsertSettingRequest {
    pub value: String,
}

#[utoipa::path(
    get,
    path = "/api/admin/plugins/{plugin_id}/settings/{key}",
    responses(
        (status = 200, description = "Setting value", body = PluginSettingView),
        (status = 404, description = "Setting not present"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "plugins"
)]
pub async fn get_setting_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((plugin_id, key)): Path<(String, String)>,
) -> Result<Json<PluginSettingView>, ApiError> {
    require_controller(&state, &user)?;
    use execlaw_core::vault_row::VaultRowStore;
    let store = VaultRowStore::new(&state.db);
    let raw = store
        .get(Some(&plugin_id), &key)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "db_error",
            message: e.to_string(),
        })?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "plugin_setting_not_found",
            message: format!("no value stored for {plugin_id}/{key}"),
        })?;
    let value = String::from_utf8(raw).map_err(|_| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "plugin_setting_not_utf8",
        message: format!("setting {plugin_id}/{key} is not valid UTF-8"),
    })?;
    Ok(Json(PluginSettingView {
        plugin_id,
        key,
        value,
    }))
}

#[utoipa::path(
    put,
    path = "/api/admin/plugins/{plugin_id}/settings/{key}",
    request_body = UpsertSettingRequest,
    responses(
        (status = 200, description = "Setting saved", body = PluginSettingView),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "plugins"
)]
pub async fn put_setting_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((plugin_id, key)): Path<(String, String)>,
    Json(req): Json<UpsertSettingRequest>,
) -> Result<Json<PluginSettingView>, ApiError> {
    require_controller(&state, &user)?;
    use execlaw_core::vault_row::VaultRowStore;
    let store = VaultRowStore::new(&state.db);
    let now = chrono::Utc::now().timestamp();
    store
        .put(Some(&plugin_id), &key, req.value.as_bytes(), now)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "db_error",
            message: e.to_string(),
        })?;
    Ok(Json(PluginSettingView {
        plugin_id,
        key,
        value: req.value,
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
            message: "only a Controller can manage plugin settings".into(),
        }),
    }
}

pub fn plugin_settings_admin_router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/admin/plugins/{plugin_id}/settings/{key}",
            get(get_setting_handler),
        )
        .route(
            "/api/admin/plugins/{plugin_id}/settings/{key}",
            put(put_setting_handler),
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

    #[tokio::test]
    async fn put_then_get_round_trips_setting() {
        let state = test_app_state();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let body = serde_json::json!({"value": "[\"gmail\",\"calendar\"]"}).to_string();
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/plugins/google-apps/settings/enabled_modules")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/plugins/google-apps/settings/enabled_modules")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["plugin_id"], "google-apps");
        assert_eq!(v["key"], "enabled_modules");
        assert_eq!(v["value"], "[\"gmail\",\"calendar\"]");
    }

    #[tokio::test]
    async fn get_missing_setting_returns_404() {
        let state = test_app_state();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/plugins/google-apps/settings/never-saved")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unauthenticated_request_is_rejected() {
        let state = test_app_state();
        let app = build_router(state);
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/plugins/google-apps/settings/x")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        );
    }
}
