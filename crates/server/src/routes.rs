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
use execlaw_container_manager::{HardwareProfile, detect_sysfs};
use execlaw_core::{Database, DbConfig};
use execlaw_vault::{hash_password, verify_password};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::path::Path;
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
    /// Login handle — required, normalized to lowercase server-side
    /// before insert (3-32 chars, [a-z0-9_-]).
    pub username: String,
    pub admin_password: String,
    /// Operator's display name — surfaces in the SPA's bottom-of-
    /// sidebar affordance and in the controller-thread's "logged in
    /// as" label.
    pub display_name: String,
    /// Optional email. Currently unused by core flows; reserved for
    /// the Phase-7 invite + password-reset flows.
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SetupResponse {
    pub principal_id: String,
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct LoginRequest {
    /// Login handle — case-insensitive; normalized server-side.
    pub username: String,
    pub admin_password: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LoginResponse {
    pub access_token: String,
    pub refresh_token: String,
}

/// Shape returned by `GET /api/admin/me` — the current logged-in
/// user's profile so the SPA can render the bottom-of-sidebar
/// "⚙ user@email" affordance.
#[derive(Debug, Serialize, ToSchema)]
pub struct MeResponse {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    pub role: String,
    pub last_login_at: Option<i64>,
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

/// Mirror the admin password hash into vault_secrets so any
/// pre-Phase-6 caller still reading from there sees a value. The
/// `users` table is the source of truth — this mirror disappears
/// when the last vault_secrets reader is migrated.
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

/// Mirror the controller principal id alongside the password hash.
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

/// `GET /api/ping` — plain-text setup-status probe the SPA hits on
/// boot to decide between routing to the setup wizard or the login
/// page. Returns the literal text `setup` (no controller user yet)
/// or `pong` (a controller user exists; SPA shows login).
///
/// Deliberately NOT JSON — the SPA wants a single byte-string check
/// without parsing.
#[utoipa::path(
    get,
    path = "/api/ping",
    responses(
        (status = 200, description = "Plain-text 'setup' (uninitialized) or 'pong' (initialized)", content_type = "text/plain"),
    ),
    tag = "meta"
)]
pub async fn ping(State(state): State<AppState>) -> impl IntoResponse {
    use execlaw_core::users::UserStore;
    let body = match UserStore::new(&state.db).any_exist() {
        Ok(true) => "pong",
        Ok(false) => "setup",
        Err(_) => "setup",
    };
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
}

/// `GET /api/admin/me` — return the current logged-in user's
/// profile. Used by the SPA to render the bottom-of-sidebar
/// affordance ("⚙ user@email") and to know the role for any
/// role-gated UI (Phase 7+).
#[utoipa::path(
    get,
    path = "/api/admin/me",
    responses(
        (status = 200, description = "Logged-in user profile", body = MeResponse),
        (status = 401, description = "Missing or invalid Authorization header"),
    ),
    security(("bearer_jwt" = [])),
    tag = "auth"
)]
pub async fn admin_me(
    user: crate::auth_extract::AuthedUser,
) -> Json<MeResponse> {
    Json(MeResponse {
        user_id: user.user_id,
        username: user.username,
        display_name: user.display_name,
        email: user.email,
        role: user.role.as_str().to_owned(),
        last_login_at: user.last_login_at,
    })
}

/// `GET /api/admin/hardware` — return the live hardware profile
/// from a fresh sysfs scan (Tier-1 detection per §5.3).
///
/// On Linux this hits `/sys/class/drm/card*/device/`; on non-Linux
/// hosts it returns an empty `HardwareProfile` (the control plane
/// is meant to run inside a Linux container — bare-Windows dev runs
/// are a known degenerate path).
#[utoipa::path(
    get,
    path = "/api/admin/hardware",
    responses(
        (status = 200, description = "Detected GPU + CPU + memory profile (sysfs Tier-1)"),
    ),
    tag = "admin"
)]
pub async fn admin_hardware() -> impl IntoResponse {
    let profile: HardwareProfile = detect_sysfs(Path::new("/sys"));
    (StatusCode::OK, Json(profile)).into_response()
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
    use execlaw_core::users::{UserRole, UserRow, UserStore, normalize_username};

    if req.admin_password.len() < 8 {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "password_too_short",
            message: "admin_password must be at least 8 characters".into(),
        });
    }
    if req.display_name.trim().is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "display_name_required",
            message: "display_name must not be empty".into(),
        });
    }
    let username = normalize_username(&req.username).map_err(|msg| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "username_invalid",
        message: msg.into(),
    })?;

    let users = UserStore::new(&state.db);
    if users.any_exist().map_err(ApiError::from)? {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "already_initialized",
            message: "a controller user already exists; use /api/login".into(),
        });
    }

    let hash = hash_password(&req.admin_password)?;
    let principal_id = format!("controller-{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now().timestamp();
    users
        .insert(&UserRow {
            user_id: principal_id.clone(),
            username,
            display_name: req.display_name.trim().to_owned(),
            email: req.email.clone().filter(|s| !s.trim().is_empty()),
            password_hash: hash.clone(),
            role: UserRole::Controller,
            created_at: now,
            last_login_at: Some(now),
        })
        .map_err(ApiError::from)?;

    // Mirror the admin password hash into vault_secrets for back-
    // compat with any callers still reading via `read_admin_hash`.
    // The `users` table is now the source of truth; this mirror
    // disappears in a follow-up cleanup pass.
    write_admin_hash(&state.db, &hash)?;
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
    use execlaw_core::users::{UserStore, normalize_username};

    let users = UserStore::new(&state.db);

    // Short-circuit "no users yet" → 409 not_initialized so the SPA
    // can route to /setup.
    if !users.any_exist().map_err(ApiError::from)? {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "not_initialized",
            message: "no controller user exists yet; run /api/setup".into(),
        });
    }

    // Validate the username shape *before* lookup so a garbage payload
    // can't probe the DB. Validation failures collapse into the same
    // 401 as a wrong username/password — never leak which half failed.
    let normalized = normalize_username(&req.username).ok();
    let user = match normalized {
        Some(name) => users.get_by_username(&name).map_err(ApiError::from)?,
        None => None,
    };
    let user = match user {
        Some(u) => u,
        None => {
            return Err(ApiError {
                status: StatusCode::UNAUTHORIZED,
                code: "bad_credentials",
                message: "incorrect username or password".into(),
            });
        }
    };
    if !verify_password(&req.admin_password, &user.password_hash)? {
        return Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            code: "bad_credentials",
            message: "incorrect username or password".into(),
        });
    }

    // Best-effort: stamp last_login_at. Failure here doesn't block login.
    let _ = users.touch_login(&user.user_id, chrono::Utc::now().timestamp());

    let session_id = uuid::Uuid::new_v4().to_string();
    let access = state.signer.issue_access_token(
        &user.user_id,
        &session_id,
        state.config.access_token_ttl_secs,
    )?;
    let refresh = state.refresh_store.issue(
        &user.user_id,
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
        .route("/api/ping", get(ping))
        .route("/api/admin/me", get(admin_me))
        .route("/api/admin/hardware", get(admin_hardware))
        .route("/api/setup", post(setup))
        .route("/api/login", post(login))
        .route("/api/token/refresh", post(refresh))
        .route("/api/logout", post(logout))
        .route(
            "/api/chats/{conversation_id}/messages",
            post(crate::chats::send_message).get(crate::chats::list_messages),
        )
        .route(
            "/api/chats/{conversation_id}",
            axum::routing::patch(crate::chats::patch_thread),
        )
        .route("/api/chats", get(crate::chats::list_threads))
        .route("/api/stream", get(crate::events::stream_handler))
        .merge(crate::plugins::plugins_router())
        .merge(crate::approvals::approvals_router())
        .merge(crate::observability::observability_router())
        .merge(crate::deployments::deployments_router())
        .merge(crate::users::users_router())
        .merge(crate::docs::docs_router())
        .with_state(state)
}

/// Build a fresh `AppState` for a unit test (in-memory DB, freshly
/// migrated, zero-TTL nothing).
pub fn test_app_state() -> AppState {
    use execlaw_core::MigrationRunner;
    use execlaw_plugin_host::{HookRegistry, PluginHost};
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    // Per-test temp dir for staged plugins. Tests that exercise the
    // install path create a TempDir explicitly; the default one is
    // just a unique path under std::env::temp_dir so this fn stays sync.
    let stage_root = std::env::temp_dir().join(format!(
        "execlaw-test-plugins-{}",
        uuid::Uuid::new_v4()
    ));
    AppState {
        db: db.clone(),
        config: Arc::new(ServerConfig::default()),
        signer: Arc::new(JwtSigner::generate("execlaw-test".into())),
        refresh_store: Arc::new(RefreshStore::new()),
        events: crate::events::EventBus::new(),
        // Tests use a deterministic HMAC key so replay works end-to-end.
        event_log_hmac_key: Some(Arc::new(
            b"execlaw-test-hmac-key-32-bytes!!".to_vec(),
        )),
        inference: None,
        plugin_host: PluginHost::new(db, HookRegistry::new(), stage_root),
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

    /// Helper for tests: full setup payload with the now-required
    /// username + display_name.
    fn setup_body(password: &str, display_name: &str) -> serde_json::Value {
        serde_json::json!({
            "username": "tester",
            "admin_password": password,
            "display_name": display_name,
        })
    }

    /// Login payload — paired with the canonical setup_body username.
    fn login_body(username: &str, password: &str) -> serde_json::Value {
        serde_json::json!({
            "username": username,
            "admin_password": password,
        })
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
            setup_body("hunter2-longer", "Justin"),
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
            setup_body("another-longer", "Other"),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // Login with wrong password: 401.
        let (status, _) = send_json(
            &app,
            Method::POST,
            "/api/login",
            login_body("tester", "nope"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Login with wrong username: 401.
        let (status, body) = send_json(
            &app,
            Method::POST,
            "/api/login",
            login_body("nobody", "hunter2-longer"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "bad_credentials");

        // Login with right username + password: 200. Also verifies
        // username case-insensitivity (input "TESTER" → matches stored "tester").
        let (status, body) = send_json(
            &app,
            Method::POST,
            "/api/login",
            login_body("TESTER", "hunter2-longer"),
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
            setup_body("x", "Justin"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn setup_rejects_empty_display_name() {
        let app = build_router(test_app_state());
        let (status, body) = send_json(
            &app,
            Method::POST,
            "/api/setup",
            serde_json::json!({
                "username": "tester",
                "admin_password": "hunter2-longer",
                "display_name": "  ",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "display_name_required");
    }

    #[tokio::test]
    async fn ping_is_setup_before_setup_then_pong_after() {
        let app = build_router(test_app_state());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/ping")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"setup");

        // After setup, ping returns "pong".
        let _ = send_json(&app, Method::POST, "/api/setup", setup_body("hunter2-longer", "J")).await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/ping")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"pong");
    }

    #[tokio::test]
    async fn admin_me_requires_auth() {
        let app = build_router(test_app_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/admin/me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_me_returns_logged_in_user_profile() {
        let app = build_router(test_app_state());
        // Setup then login to get a fresh access token.
        let (_, body) = send_json(
            &app,
            Method::POST,
            "/api/setup",
            serde_json::json!({
                "username": "JLong",
                "admin_password": "hunter2-longer",
                "display_name": "Justin Long",
                "email": "justin@example.com"
            }),
        )
        .await;
        let token = body["access_token"].as_str().unwrap().to_owned();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/me")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let me: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(me["display_name"], "Justin Long");
        assert_eq!(me["email"], "justin@example.com");
        assert_eq!(me["role"], "controller");
        // username is stored lowercased even though "JLong" was sent.
        assert_eq!(me["username"], "jlong");
        assert!(me["user_id"].as_str().unwrap().starts_with("controller-"));
    }

    #[tokio::test]
    async fn login_without_setup_returns_conflict() {
        let app = build_router(test_app_state());
        let (status, _) = send_json(
            &app,
            Method::POST,
            "/api/login",
            login_body("tester", "hunter2-longer"),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    /// Setup must reject empty / malformed usernames before writing
    /// anything. Pairs with the core-side `normalize_username` tests.
    #[tokio::test]
    async fn setup_rejects_invalid_username() {
        let app = build_router(test_app_state());
        for bad in ["", "  ", "ab", "has space", "with.dot"] {
            let (status, body) = send_json(
                &app,
                Method::POST,
                "/api/setup",
                serde_json::json!({
                    "username": bad,
                    "admin_password": "hunter2-longer",
                    "display_name": "J",
                }),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "username '{bad}' should be rejected; body was {body}"
            );
            assert_eq!(body["error"]["code"], "username_invalid");
        }
    }

    /// /api/login must NOT distinguish "wrong username" from "wrong
    /// password" — both routes must collapse to the same 401 +
    /// `bad_credentials` so an attacker can't enumerate usernames.
    #[tokio::test]
    async fn login_does_not_leak_username_existence() {
        let app = build_router(test_app_state());
        // Establish a known user.
        let (_, _) = send_json(
            &app,
            Method::POST,
            "/api/setup",
            serde_json::json!({
                "username": "tester",
                "admin_password": "hunter2-longer",
                "display_name": "J",
            }),
        )
        .await;

        let (s1, b1) = send_json(
            &app,
            Method::POST,
            "/api/login",
            login_body("tester", "wrong-password"),
        )
        .await;
        let (s2, b2) = send_json(
            &app,
            Method::POST,
            "/api/login",
            login_body("does-not-exist", "anything"),
        )
        .await;
        assert_eq!(s1, s2);
        assert_eq!(b1["error"]["code"], b2["error"]["code"]);
        assert_eq!(b1["error"]["code"], "bad_credentials");
    }
}
