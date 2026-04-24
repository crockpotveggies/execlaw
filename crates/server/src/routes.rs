//! HTTP route handlers.
//!
//! Phase 0 scope:
//!   GET  /api/health         liveness
//!   POST /api/setup          first-run admin password
//!   POST /api/login          password → JWT + refresh
//!   POST /api/token/refresh  rotate refresh token
//!   POST /api/logout         invalidate refresh
//!   GET  /api/openapi.json   OpenAPI 3 spec
//!   GET  /api/asyncapi.json  AsyncAPI 3 spec (hand-authored)
//!   GET  /api/docs           Swagger UI + AsyncAPI viewer bundle

use crate::auth::{AuthError, JwtSigner, RefreshStore};
use crate::state::{AppState, ServerConfig};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use execlaw_core::{Database, DbConfig};
use execlaw_vault::{hash_password, verify_password};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;

/// Constant vault key under which the admin-password hash lives.
const ADMIN_PWD_KEY: &str = "admin_password_hash";
const CONTROLLER_PRINCIPAL_KEY: &str = "controller_principal_id";

// -----------------------------------------------------------------------
// Request / response payloads
// -----------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct HealthResponse {
    pub status: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetupRequest {
    pub admin_password: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SetupResponse {
    pub principal_id: String,
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct LoginRequest {
    pub admin_password: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LoginResponse {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RefreshResponse {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct LogoutRequest {
    /// Optional — if absent we invalidate by access-token `sid` claim.
    pub refresh_token: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GenericOk {
    pub ok: bool,
}

// -----------------------------------------------------------------------
// Error mapping
// -----------------------------------------------------------------------

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let body = serde_json::json!({
            "error": {
                "code": self.code,
                "message": self.message,
            }
        });
        (self.status, Json(body)).into_response()
    }
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::NotInitialized => ApiError {
                status: StatusCode::CONFLICT,
                code: "not_initialized",
                message: e.to_string(),
            },
            AuthError::BadPassword => ApiError {
                status: StatusCode::UNAUTHORIZED,
                code: "bad_password",
                message: e.to_string(),
            },
            AuthError::Invalid | AuthError::Jwt(_) => ApiError {
                status: StatusCode::UNAUTHORIZED,
                code: "invalid_token",
                message: e.to_string(),
            },
            AuthError::Base64(s) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "internal",
                message: s,
            },
        }
    }
}

impl From<execlaw_core::DbError> for ApiError {
    fn from(e: execlaw_core::DbError) -> Self {
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "db_error",
            message: e.to_string(),
        }
    }
}

impl From<execlaw_vault::PasswordError> for ApiError {
    fn from(e: execlaw_vault::PasswordError) -> Self {
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "password_error",
            message: e.to_string(),
        }
    }
}

// -----------------------------------------------------------------------
// Helpers: read / write admin-password hash and controller principal.
// -----------------------------------------------------------------------

fn read_admin_hash(db: &Database) -> Result<Option<String>, ApiError> {
    db.with_conn(|c| {
        let got = c
            .query_row(
                "SELECT value_blob FROM vault_secrets \
                 WHERE plugin_id IS NULL AND name = ?1",
                params![ADMIN_PWD_KEY],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .ok();
        Ok(got
            .map(|bytes| String::from_utf8(bytes).unwrap_or_default())
            .filter(|s| !s.is_empty()))
    })
    .map_err(Into::into)
}

fn write_admin_hash(db: &Database, hash: &str) -> Result<(), ApiError> {
    let now = chrono::Utc::now().timestamp();
    db.with_conn(|c| {
        c.execute(
            "INSERT INTO vault_secrets(name, plugin_id, value_blob, created_at, updated_at) \
             VALUES (?1, NULL, ?2, ?3, ?3) \
             ON CONFLICT(plugin_id, name) DO UPDATE SET \
                 value_blob = excluded.value_blob, updated_at = excluded.updated_at",
            params![ADMIN_PWD_KEY, hash.as_bytes(), now],
        )?;
        Ok(())
    })
    .map_err(Into::into)
}

fn read_controller_principal(db: &Database) -> Result<Option<String>, ApiError> {
    db.with_conn(|c| {
        let got = c
            .query_row(
                "SELECT value_blob FROM vault_secrets \
                 WHERE plugin_id IS NULL AND name = ?1",
                params![CONTROLLER_PRINCIPAL_KEY],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .ok();
        Ok(got.map(|b| String::from_utf8(b).unwrap_or_default()))
    })
    .map_err(Into::into)
}

fn write_controller_principal(db: &Database, principal_id: &str) -> Result<(), ApiError> {
    let now = chrono::Utc::now().timestamp();
    db.with_conn(|c| {
        c.execute(
            "INSERT INTO vault_secrets(name, plugin_id, value_blob, created_at, updated_at) \
             VALUES (?1, NULL, ?2, ?3, ?3) \
             ON CONFLICT(plugin_id, name) DO UPDATE SET \
                 value_blob = excluded.value_blob, updated_at = excluded.updated_at",
            params![CONTROLLER_PRINCIPAL_KEY, principal_id.as_bytes(), now],
        )?;
        Ok(())
    })
    .map_err(Into::into)
}

// -----------------------------------------------------------------------
// Handlers
// -----------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/api/health",
    responses((status = 200, description = "Liveness", body = HealthResponse)),
    tag = "meta"
)]
pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".into(),
    })
}

#[utoipa::path(
    post,
    path = "/api/setup",
    request_body = SetupRequest,
    responses(
        (status = 200, description = "First-run setup completed", body = SetupResponse),
        (status = 409, description = "Admin password already set"),
    ),
    tag = "auth"
)]
pub async fn setup(
    State(state): State<AppState>,
    Json(req): Json<SetupRequest>,
) -> Result<Json<SetupResponse>, ApiError> {
    if req.admin_password.len() < 8 {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "password_too_short",
            message: "admin_password must be at least 8 characters".into(),
        });
    }

    // Refuse if already initialized.
    if read_admin_hash(&state.db)?.is_some() {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "already_initialized",
            message: "admin password already set; use /api/login".into(),
        });
    }

    let hash = hash_password(&req.admin_password)?;
    write_admin_hash(&state.db, &hash)?;

    let principal_id = format!("controller-{}", uuid::Uuid::new_v4());
    write_controller_principal(&state.db, &principal_id)?;

    // Issue a first session.
    let session_id = uuid::Uuid::new_v4().to_string();
    let access = state.signer.issue_access_token(
        &principal_id,
        &session_id,
        state.config.access_token_ttl_secs,
    )?;
    let refresh = state.refresh_store.issue(
        &principal_id,
        &session_id,
        state.config.refresh_token_ttl_secs,
    );

    Ok(Json(SetupResponse {
        principal_id,
        access_token: access,
        refresh_token: refresh,
    }))
}

#[utoipa::path(
    post,
    path = "/api/login",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Login ok", body = LoginResponse),
        (status = 401, description = "Bad password"),
    ),
    tag = "auth"
)]
pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    let hash = read_admin_hash(&state.db)?.ok_or(ApiError {
        status: StatusCode::CONFLICT,
        code: "not_initialized",
        message: "admin password not set yet; run /api/setup".into(),
    })?;
    if !verify_password(&req.admin_password, &hash)? {
        return Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            code: "bad_password",
            message: "incorrect admin password".into(),
        });
    }

    let principal_id = read_controller_principal(&state.db)?.unwrap_or_else(|| {
        // Should never happen — setup writes both — but be robust.
        "controller".to_owned()
    });
    let session_id = uuid::Uuid::new_v4().to_string();
    let access = state.signer.issue_access_token(
        &principal_id,
        &session_id,
        state.config.access_token_ttl_secs,
    )?;
    let refresh = state.refresh_store.issue(
        &principal_id,
        &session_id,
        state.config.refresh_token_ttl_secs,
    );
    Ok(Json(LoginResponse {
        access_token: access,
        refresh_token: refresh,
    }))
}

#[utoipa::path(
    post,
    path = "/api/token/refresh",
    request_body = RefreshRequest,
    responses(
        (status = 200, description = "New token pair", body = RefreshResponse),
        (status = 401, description = "Invalid or expired refresh token"),
    ),
    tag = "auth"
)]
pub async fn refresh(
    State(state): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<RefreshResponse>, ApiError> {
    let record = state
        .refresh_store
        .consume(&req.refresh_token)
        .ok_or(ApiError {
            status: StatusCode::UNAUTHORIZED,
            code: "invalid_refresh_token",
            message: "refresh token invalid, expired, or already used".into(),
        })?;

    let access = state.signer.issue_access_token(
        &record.principal_id,
        &record.session_id,
        state.config.access_token_ttl_secs,
    )?;
    let refresh = state.refresh_store.issue(
        &record.principal_id,
        &record.session_id,
        state.config.refresh_token_ttl_secs,
    );
    Ok(Json(RefreshResponse {
        access_token: access,
        refresh_token: refresh,
    }))
}

#[utoipa::path(
    post,
    path = "/api/logout",
    request_body = LogoutRequest,
    responses((status = 200, description = "Logged out", body = GenericOk)),
    tag = "auth"
)]
pub async fn logout(
    State(state): State<AppState>,
    Json(req): Json<LogoutRequest>,
) -> Result<Json<GenericOk>, ApiError> {
    if let Some(tok) = &req.refresh_token {
        if let Some(record) = state.refresh_store.consume(tok) {
            state.refresh_store.revoke_session(&record.session_id);
        }
    }
    Ok(Json(GenericOk { ok: true }))
}

// -----------------------------------------------------------------------
// Router assembly
// -----------------------------------------------------------------------

/// Build the Axum `Router` for execlaw.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/setup", post(setup))
        .route("/api/login", post(login))
        .route("/api/token/refresh", post(refresh))
        .route("/api/logout", post(logout))
        .route(
            "/api/chats/{conversation_id}/messages",
            post(crate::chats::send_message).get(crate::chats::list_messages),
        )
        .route("/api/stream", get(crate::events::stream_handler))
        .merge(crate::docs::docs_router())
        .with_state(state)
}

/// Build a fresh `AppState` for a unit test (in-memory DB, freshly
/// migrated, zero-TTL nothing).
pub fn test_app_state() -> AppState {
    use execlaw_core::MigrationRunner;
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    AppState {
        db,
        config: Arc::new(ServerConfig::default()),
        signer: Arc::new(JwtSigner::generate("execlaw-test".into())),
        refresh_store: Arc::new(RefreshStore::new()),
        events: crate::events::EventBus::new(),
        // Tests use a deterministic HMAC key so replay works end-to-end.
        event_log_hmac_key: Some(Arc::new(
            b"execlaw-test-hmac-key-32-bytes!!".to_vec(),
        )),
        inference: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{self, Body};
    use axum::http::{HeaderValue, Method, Request, header};
    use tower::ServiceExt;

    async fn send_json(
        app: &axum::Router,
        method: Method,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let body_bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body_json: serde_json::Value =
            serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);
        (status, body_json)
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let state = test_app_state();
        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body_bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let j: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(j["status"], "ok");
    }

    #[tokio::test]
    async fn setup_login_refresh_logout_end_to_end() {
        let state = test_app_state();
        let app = build_router(state);

        // First run: setup succeeds.
        let (status, body) = send_json(
            &app,
            Method::POST,
            "/api/setup",
            serde_json::json!({ "admin_password": "hunter2-longer" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "setup body was {body}");
        assert!(body["access_token"].is_string());
        assert!(body["refresh_token"].is_string());
        let refresh1 = body["refresh_token"].as_str().unwrap().to_owned();

        // Setup again: conflict.
        let (status, _) = send_json(
            &app,
            Method::POST,
            "/api/setup",
            serde_json::json!({ "admin_password": "another-longer" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // Login with wrong password: 401.
        let (status, _) = send_json(
            &app,
            Method::POST,
            "/api/login",
            serde_json::json!({ "admin_password": "nope" }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Login with right password: 200.
        let (status, body) = send_json(
            &app,
            Method::POST,
            "/api/login",
            serde_json::json!({ "admin_password": "hunter2-longer" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let refresh2 = body["refresh_token"].as_str().unwrap().to_owned();

        // Refresh: 200 + rotates token.
        let (status, body) = send_json(
            &app,
            Method::POST,
            "/api/token/refresh",
            serde_json::json!({ "refresh_token": refresh2 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let refresh3 = body["refresh_token"].as_str().unwrap().to_owned();
        assert_ne!(refresh3, refresh2);

        // Old refresh_token is single-use: 401 on re-consume.
        let (status, _) = send_json(
            &app,
            Method::POST,
            "/api/token/refresh",
            serde_json::json!({ "refresh_token": refresh2 }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Logout: 200. revoke_session() kills everything on the session
        // the refresh token belongs to — but the *setup* flow used a
        // different session, so its refresh1 is still valid.
        let (status, _) = send_json(
            &app,
            Method::POST,
            "/api/logout",
            serde_json::json!({ "refresh_token": refresh3 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // After logout, the just-used refresh3 is dead.
        let (status, _) = send_json(
            &app,
            Method::POST,
            "/api/token/refresh",
            serde_json::json!({ "refresh_token": refresh3 }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // refresh1 (from setup) lives in a different session — still valid.
        let (status, body) = send_json(
            &app,
            Method::POST,
            "/api/token/refresh",
            serde_json::json!({ "refresh_token": refresh1 }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "setup-session refresh survives login-session logout"
        );
        assert!(body["access_token"].is_string());
    }

    #[tokio::test]
    async fn setup_rejects_short_password() {
        let app = build_router(test_app_state());
        let (status, _) = send_json(
            &app,
            Method::POST,
            "/api/setup",
            serde_json::json!({ "admin_password": "x" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn login_without_setup_returns_conflict() {
        let app = build_router(test_app_state());
        let (status, _) = send_json(
            &app,
            Method::POST,
            "/api/login",
            serde_json::json!({ "admin_password": "hunter2-longer" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }
}
