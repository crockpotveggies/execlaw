//! C3 — operator-facing Research settings.
//!
//! Backs the SPA's `Settings → Research` page. Singleton row in
//! `config_research` (seeded by migration 0027) captures every
//! research subsystem default the operator can tune. Per-conversation
//! and per-job overrides land on `state_conversations.settings_json`
//! / the tool-args layer respectively (C4-C5).
//!
//! Routes:
//!   * `GET  /api/admin/settings/research` — read (any authed user).
//!   * `PUT  /api/admin/settings/research` — write (Controller only).

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use execlaw_core::audit::AuditStore;
use execlaw_core::research::{
    PhaseGates, ResearchConfig, ResearchConfigStore, ResearchConfigUpdate, ResearchError,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct ResearchSettingsView {
    pub max_wall_clock_minutes: u32,
    pub max_total_tokens: u32,
    pub max_subqueries: u32,
    pub parallel_workers: u32,
    pub max_urls_per_subquery: u32,
    pub max_pages_total: u32,
    pub auto_cancel_after_idle_secs: u32,
    /// One of `none` / `plan_only` / `every_phase`. Default `plan_only`.
    pub phase_gates: String,
    pub default_search_provider: Option<String>,
    pub updated_at: i64,
}

impl From<ResearchConfig> for ResearchSettingsView {
    fn from(c: ResearchConfig) -> Self {
        Self {
            max_wall_clock_minutes: c.max_wall_clock_minutes,
            max_total_tokens: c.max_total_tokens,
            max_subqueries: c.max_subqueries,
            parallel_workers: c.parallel_workers,
            max_urls_per_subquery: c.max_urls_per_subquery,
            max_pages_total: c.max_pages_total,
            auto_cancel_after_idle_secs: c.auto_cancel_after_idle_secs,
            phase_gates: c.phase_gates.as_str().to_owned(),
            default_search_provider: c.default_search_provider,
            updated_at: c.updated_at,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateResearchSettingsRequest {
    #[serde(default)]
    pub max_wall_clock_minutes: Option<u32>,
    #[serde(default)]
    pub max_total_tokens: Option<u32>,
    #[serde(default)]
    pub max_subqueries: Option<u32>,
    #[serde(default)]
    pub parallel_workers: Option<u32>,
    #[serde(default)]
    pub max_urls_per_subquery: Option<u32>,
    #[serde(default)]
    pub max_pages_total: Option<u32>,
    #[serde(default)]
    pub auto_cancel_after_idle_secs: Option<u32>,
    /// One of `none` / `plan_only` / `every_phase`.
    #[serde(default)]
    pub phase_gates: Option<String>,
    /// Outer optional = "patch present?", `Some(None)` = clear to
    /// inherit from Settings → Search; `Some(Some("brave"))` sets it.
    /// Strings on the wire — `null` is treated as "clear".
    #[serde(default, deserialize_with = "deser_optional_optional")]
    pub default_search_provider: Option<Option<String>>,
}

/// Custom deserializer so the JSON shape `{"default_search_provider":
/// null}` round-trips as `Some(None)` (clear) rather than `None`
/// (untouched). Without this, the operator can't blank the column
/// from the SPA.
fn deser_optional_optional<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: serde_json::Value = serde::Deserialize::deserialize(deserializer)?;
    match v {
        serde_json::Value::Null => Ok(Some(None)),
        serde_json::Value::String(s) => Ok(Some(Some(s))),
        _ => Err(serde::de::Error::custom(
            "default_search_provider must be a string or null",
        )),
    }
}

impl From<ResearchError> for ApiError {
    fn from(err: ResearchError) -> Self {
        match err {
            ResearchError::Db(e) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "db_error",
                message: e.to_string(),
            },
            ResearchError::Sqlite(e) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "db_error",
                message: e.to_string(),
            },
            ResearchError::Invalid(msg) => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_research_setting",
                message: msg,
            },
            ResearchError::NotFound(msg) => ApiError {
                status: StatusCode::NOT_FOUND,
                code: "research_not_found",
                message: msg,
            },
            ResearchError::Encoding(msg) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "research_encoding_error",
                message: msg,
            },
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/settings/research",
    responses((status = 200, description = "Research subsystem defaults", body = ResearchSettingsView)),
    security(("bearer_jwt" = [])),
    tag = "settings"
)]
pub async fn get_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<ResearchSettingsView>, ApiError> {
    let cfg = ResearchConfigStore::new(&state.db).get()?;
    Ok(Json(cfg.into()))
}

#[utoipa::path(
    put,
    path = "/api/admin/settings/research",
    request_body = UpdateResearchSettingsRequest,
    responses(
        (status = 200, description = "Updated", body = ResearchSettingsView),
        (status = 400, description = "Invalid value (out of allowed range or vocabulary)"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "settings"
)]
pub async fn put_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<UpdateResearchSettingsRequest>,
) -> Result<Json<ResearchSettingsView>, ApiError> {
    require_controller(&state, &user)?;
    // Phase-gate vocabulary check at the boundary.
    let phase_gates = match req.phase_gates.as_deref() {
        Some(s) => match PhaseGates::parse(s) {
            Some(g) => Some(g),
            None => {
                return Err(ApiError {
                    status: StatusCode::BAD_REQUEST,
                    code: "invalid_phase_gates",
                    message: format!(
                        "phase_gates must be one of none / plan_only / every_phase (got '{s}')"
                    ),
                });
            }
        },
        None => None,
    };
    let patch = ResearchConfigUpdate {
        max_wall_clock_minutes: req.max_wall_clock_minutes,
        max_total_tokens: req.max_total_tokens,
        max_subqueries: req.max_subqueries,
        parallel_workers: req.parallel_workers,
        max_urls_per_subquery: req.max_urls_per_subquery,
        max_pages_total: req.max_pages_total,
        auto_cancel_after_idle_secs: req.auto_cancel_after_idle_secs,
        phase_gates,
        default_search_provider: req.default_search_provider,
    };
    let now = chrono::Utc::now().timestamp();
    let store = ResearchConfigStore::new(&state.db);
    let prior = store.get().ok();
    let saved = store.update(&patch, now)?;
    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "config_research",
        "research",
        prior
            .as_ref()
            .map(|p| {
                serde_json::json!({
                    "max_wall_clock_minutes": p.max_wall_clock_minutes,
                    "max_total_tokens": p.max_total_tokens,
                    "phase_gates": p.phase_gates.as_str(),
                })
            })
            .as_ref(),
        Some(&serde_json::json!({
            "max_wall_clock_minutes": saved.max_wall_clock_minutes,
            "max_total_tokens": saved.max_total_tokens,
            "phase_gates": saved.phase_gates.as_str(),
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
            message: "only a Controller can change research settings".into(),
        }),
    }
}

pub fn settings_research_router() -> Router<AppState> {
    Router::new().route(
        "/api/admin/settings/research",
        get(get_handler).put(put_handler),
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
    async fn get_returns_seeded_defaults() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/settings/research")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["max_wall_clock_minutes"], 30);
        assert_eq!(v["max_subqueries"], 12);
        assert_eq!(v["parallel_workers"], 3);
        assert_eq!(v["phase_gates"], "plan_only");
    }

    #[tokio::test]
    async fn put_round_trips_individual_fields() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/settings/research")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "parallel_workers": 5,
                    "phase_gates": "none",
                    "default_search_provider": "brave",
                })
                .to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["parallel_workers"], 5);
        assert_eq!(v["phase_gates"], "none");
        assert_eq!(v["default_search_provider"], "brave");
        // Untouched field keeps its seeded default.
        assert_eq!(v["max_subqueries"], 12);
    }

    #[tokio::test]
    async fn put_rejects_garbage_phase_gates() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/settings/research")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"phase_gates": "always"}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn put_rejects_out_of_range_numbers() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/settings/research")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"parallel_workers": 0}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn put_clears_default_search_provider_with_null() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        // First set a value.
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/settings/research")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"default_search_provider": "brave"}).to_string(),
            ))
            .unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();
        // Then clear it via null.
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/settings/research")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"default_search_provider": null}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["default_search_provider"].is_null());
    }
}
