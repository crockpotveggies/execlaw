//! Phase 14 — operator-facing General settings.
//!
//! Backs the SPA's `Settings → General` page. Two fields today:
//!
//!   * `start_on_boot` — should the host service start at OS boot?
//!     Edits flip the persisted value; the service-manager
//!     registration is updated on the next `execlaw service install`
//!     (we don't try to re-register from the running process; that
//!     requires elevated privileges on Windows and bypasses the
//!     operator's expectation of explicit install verbs).
//!
//!   * `bind_address` — host:port the next service start will bind.
//!     Edits don't restart the running process; the SPA prompts the
//!     operator to run `execlaw service restart`.
//!
//! Routes:
//!   * `GET  /api/admin/settings/general` — read (any authed user).
//!   * `PUT  /api/admin/settings/general` — write (Controller only).

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use execlaw_core::audit::AuditStore;
use execlaw_core::general_settings::{
    GeneralSettings, GeneralSettingsError, GeneralSettingsStore,
    GeneralSettingsUpdate,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct GeneralSettingsView {
    pub start_on_boot: bool,
    pub bind_address: String,
    pub updated_at: i64,
    /// Whether changing `bind_address` will take effect on the next
    /// `service restart`. Always true today; the field documents the
    /// contract for the SPA's "restart required" hint.
    pub bind_address_requires_restart: bool,
    /// 2026-04-29 — global history retention. `0` = infinite (never
    /// delete); other legal values are 30 / 60 / 90 / 120. Surfaced
    /// to the SPA's Settings → General dropdown.
    pub history_retention_days: u32,
}

impl From<GeneralSettings> for GeneralSettingsView {
    fn from(s: GeneralSettings) -> Self {
        Self {
            start_on_boot: s.start_on_boot,
            bind_address: s.bind_address,
            updated_at: s.updated_at,
            bind_address_requires_restart: true,
            history_retention_days: s.history_retention_days,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateGeneralSettingsRequest {
    /// Optional — omit the field to leave it untouched. Sending
    /// `null` is treated identically.
    #[serde(default)]
    pub start_on_boot: Option<bool>,
    #[serde(default)]
    pub bind_address: Option<String>,
    /// Optional. Legal values: 0 (infinite), 30, 60, 90, 120. The
    /// store rejects anything else with `InvalidBindAddress` (the
    /// shared validation-error variant — message names the actual
    /// reason).
    #[serde(default)]
    pub history_retention_days: Option<u32>,
}

impl From<GeneralSettingsError> for ApiError {
    fn from(err: GeneralSettingsError) -> Self {
        match err {
            GeneralSettingsError::Db(e) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "db_error",
                message: e.to_string(),
            },
            GeneralSettingsError::Sqlite(e) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "db_error",
                message: e.to_string(),
            },
            GeneralSettingsError::InvalidBindAddress(msg) => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_bind_address",
                message: msg,
            },
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/settings/general",
    responses((status = 200, description = "Operator-editable general settings", body = GeneralSettingsView)),
    security(("bearer_jwt" = [])),
    tag = "settings"
)]
pub async fn get_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<GeneralSettingsView>, ApiError> {
    let store = GeneralSettingsStore::new(&state.db);
    let row = store.get().map_err(ApiError::from)?.ok_or_else(|| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "general_settings_missing",
        message: "config_general singleton row missing — migration 0017 didn't run".into(),
    })?;
    Ok(Json(row.into()))
}

#[utoipa::path(
    put,
    path = "/api/admin/settings/general",
    request_body = UpdateGeneralSettingsRequest,
    responses(
        (status = 200, description = "Updated", body = GeneralSettingsView),
        (status = 400, description = "Invalid bind_address"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "settings"
)]
pub async fn put_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<UpdateGeneralSettingsRequest>,
) -> Result<Json<GeneralSettingsView>, ApiError> {
    require_controller(&state, &user)?;
    let store = GeneralSettingsStore::new(&state.db);
    let prior = store.get().map_err(ApiError::from)?;
    let now = chrono::Utc::now().timestamp();
    let saved = store
        .update(
            &GeneralSettingsUpdate {
                start_on_boot: req.start_on_boot,
                bind_address: req.bind_address.clone(),
                // The wizard-dismissed flag has its own dedicated
                // endpoint (`POST /api/admin/setup/dismiss`); the
                // /general PUT route doesn't expose it so the SPA
                // can't accidentally clear it from the General
                // settings page.
                setup_wizard_dismissed: None,
                history_retention_days: req.history_retention_days,
            },
            now,
        )
        .map_err(ApiError::from)?;

    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "config_general",
        "general",
        prior.as_ref().map(|p| {
            serde_json::json!({
                "start_on_boot": p.start_on_boot,
                "bind_address": p.bind_address,
            })
        }).as_ref(),
        Some(&serde_json::json!({
            "start_on_boot": saved.start_on_boot,
            "bind_address": saved.bind_address,
        })),
    );

    Ok(Json(saved.into()))
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
            message: "only a Controller can change general settings".into(),
        }),
    }
}

pub fn settings_router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/admin/settings/general",
            get(get_handler).put(put_handler),
        )
        .route(
            "/api/admin/settings/hf-cache",
            get(get_hf_cache_handler).put(put_hf_cache_handler),
        )
}

// ----- Phase 14.C — operator-supplied secondary HF cache list -----

#[derive(Debug, Serialize, ToSchema)]
pub struct HfCacheView {
    /// Host paths the supervisor scans (read-only) before falling
    /// back to a network download. Order is preserved from the
    /// last write — there's no priority semantics, but ordering is
    /// the operator's choice.
    pub secondary_paths: Vec<String>,
    /// True iff the new value will take effect on the next
    /// `execlaw service restart`. We snapshot the secondary list at
    /// supervisor boot today; live reload is future work.
    pub requires_restart: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateHfCacheRequest {
    /// Replace the full list. Pass an empty array to clear.
    pub secondary_paths: Vec<String>,
}

#[utoipa::path(
    get,
    path = "/api/admin/settings/hf-cache",
    responses((status = 200, description = "Configured secondary HF cache paths", body = HfCacheView)),
    security(("bearer_jwt" = [])),
    tag = "settings"
)]
pub async fn get_hf_cache_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<HfCacheView>, ApiError> {
    let store = GeneralSettingsStore::new(&state.db);
    let paths = store.read_secondary_hf_caches().map_err(ApiError::from)?;
    Ok(Json(HfCacheView {
        secondary_paths: paths
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        requires_restart: true,
    }))
}

#[utoipa::path(
    put,
    path = "/api/admin/settings/hf-cache",
    request_body = UpdateHfCacheRequest,
    responses(
        (status = 200, description = "Saved", body = HfCacheView),
        (status = 400, description = "Invalid path (must be absolute, non-empty)"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "settings"
)]
pub async fn put_hf_cache_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<UpdateHfCacheRequest>,
) -> Result<Json<HfCacheView>, ApiError> {
    require_controller(&state, &user)?;
    let paths: Vec<std::path::PathBuf> = req
        .secondary_paths
        .iter()
        .map(std::path::PathBuf::from)
        .collect();
    let store = GeneralSettingsStore::new(&state.db);
    store
        .write_secondary_hf_caches(&paths, chrono::Utc::now().timestamp())
        .map_err(ApiError::from)?;
    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "config_general",
        "hf_secondary_caches_json",
        None,
        Some(&serde_json::json!({ "secondary_paths": &req.secondary_paths })),
    );
    Ok(Json(HfCacheView {
        secondary_paths: req.secondary_paths,
        requires_restart: true,
    }))
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
    async fn get_returns_seeded_defaults() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/settings/general")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["start_on_boot"], true);
        assert_eq!(v["bind_address"], "127.0.0.1:3030");
        assert_eq!(v["bind_address_requires_restart"], true);
    }

    #[tokio::test]
    async fn put_round_trips_bind_address() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/settings/general")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"bind_address":"0.0.0.0:9000"}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["bind_address"], "0.0.0.0:9000");
        assert_eq!(
            v["start_on_boot"], true,
            "untouched field must keep its prior value"
        );
    }

    #[tokio::test]
    async fn put_rejects_garbage_bind_address() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/settings/general")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"bind_address":"not a host port"}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn put_toggles_start_on_boot() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/settings/general")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"start_on_boot":false}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["start_on_boot"], false);
    }

    #[tokio::test]
    async fn hf_cache_get_starts_empty_and_put_round_trips() {
        // Phase 14.C — fresh DB has no secondary caches configured.
        // PUT replaces the full list; subsequent GET returns the
        // operator's saved paths.
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;

        // GET on a fresh DB returns an empty list.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/settings/hf-cache")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["secondary_paths"].as_array().unwrap().len(), 0);

        // PUT writes the list.
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/settings/hf-cache")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "secondary_paths": ["/home/me/.cache/huggingface", "/data/hf"]
                })
                .to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // GET reflects the new state.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/settings/hf-cache")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let arr = v["secondary_paths"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], "/home/me/.cache/huggingface");
        assert_eq!(arr[1], "/data/hf");
    }

    #[tokio::test]
    async fn hf_cache_put_rejects_relative_paths() {
        // Defense-in-depth — a relative path resolved from the
        // service's working directory probably isn't what the
        // operator meant. The store rejects with `invalid_bind_address`
        // (the existing GeneralSettingsError variant; we reuse it
        // for path validation since the message is descriptive).
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/settings/hf-cache")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "secondary_paths": ["./relative/path"]
                })
                .to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
