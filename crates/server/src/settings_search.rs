//! `/api/admin/search/...` — operator CRUD for the search-provider
//! registry + a test-search endpoint so the operator can verify a
//! provider's config without spinning up a research job.

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::search_resolver::construct_from_row;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{delete, get, post};
use execlaw_core::search_providers::{
    SearchProviderError, SearchProviderKind, SearchProviderRow, SearchProviderStore,
};
use execlaw_core::tool::WebSearchApi;
use execlaw_core::users::{UserRole, UserStore};
use serde::{Deserialize, Serialize};

/// Wire shape for a single provider row. Mirrors
/// `SearchProviderRow` but represents `kind` as the wire string +
/// `config_json` as a parsed serde_json::Value (so the SPA doesn't
/// have to double-decode).
#[derive(Debug, Serialize, Deserialize)]
pub struct ProviderView {
    pub kind: String,
    pub display_name: String,
    pub enabled: bool,
    pub is_default: bool,
    pub config: serde_json::Value,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Serialize)]
pub struct ProvidersListResponse {
    pub providers: Vec<ProviderView>,
}

#[derive(Debug, Deserialize)]
pub struct UpsertProviderRequest {
    pub kind: String,
    pub enabled: bool,
    pub is_default: bool,
    /// Per-kind config object. SearxNG: `{"base_url": "..."}`,
    /// Brave: `{"api_key": "..."}`, DuckDuckGo: `{}`.
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct TestSearchRequest {
    /// What to search for. The endpoint runs the active provider's
    /// `search()` against this query and returns the first few
    /// hits so the operator can confirm the provider is reachable
    /// and returning results.
    pub query: String,
}

#[derive(Debug, Serialize)]
pub struct TestSearchResponse {
    pub provider_id: String,
    pub results: Vec<TestSearchHit>,
    /// Wall-clock ms the search call took. Useful operator signal
    /// for "is this fast enough?"
    pub elapsed_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct TestSearchHit {
    pub title: String,
    pub url: String,
    pub snippet: Option<String>,
}

fn require_controller(state: &AppState, user: &AuthedUser) -> Result<(), ApiError> {
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
            message: "only a Controller can manage search providers".into(),
        }),
    }
}

fn row_to_view(row: SearchProviderRow) -> ProviderView {
    let config = serde_json::from_str(&row.config_json).unwrap_or(serde_json::Value::Null);
    ProviderView {
        kind: row.kind.as_str().to_owned(),
        display_name: row.kind.display_name().to_owned(),
        enabled: row.enabled,
        is_default: row.is_default,
        config,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn map_provider_err(e: SearchProviderError) -> ApiError {
    match e {
        SearchProviderError::NotFound(s) => ApiError {
            status: StatusCode::NOT_FOUND,
            code: "search_provider_not_found",
            message: s,
        },
        SearchProviderError::UnknownKind(s) => ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "unknown_provider_kind",
            message: format!("unknown provider kind: {s}"),
        },
        SearchProviderError::Invalid(s) => ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_provider",
            message: s,
        },
        SearchProviderError::Db(e) => ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "db_error",
            message: e.to_string(),
        },
    }
}

pub async fn list_providers_handler(
    State(state): State<AppState>,
    user: AuthedUser,
) -> Result<Json<ProvidersListResponse>, ApiError> {
    require_controller(&state, &user)?;
    let rows = SearchProviderStore::new(&state.db)
        .list_all()
        .map_err(map_provider_err)?;
    Ok(Json(ProvidersListResponse {
        providers: rows.into_iter().map(row_to_view).collect(),
    }))
}

pub async fn upsert_provider_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<UpsertProviderRequest>,
) -> Result<Json<ProviderView>, ApiError> {
    require_controller(&state, &user)?;
    let kind = SearchProviderKind::parse(&req.kind).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_provider_kind",
        message: format!("unknown provider kind: {}", req.kind),
    })?;
    let now = chrono::Utc::now().timestamp();
    let store = SearchProviderStore::new(&state.db);
    // Preserve created_at on update (the SQL ON CONFLICT branch
    // doesn't touch it, but we still need a value to pass).
    let created_at = store
        .get(kind)
        .map_err(map_provider_err)?
        .map(|r| r.created_at)
        .unwrap_or(now);
    // Serialize config back to JSON text. Reject anything that
    // isn't an object (per-kind schemas all use objects today; a
    // bare scalar or array is operator error).
    if !req.config.is_null() && !req.config.is_object() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_config_shape",
            message: "config must be a JSON object (or null/empty)".into(),
        });
    }
    let config_json = serde_json::to_string(&req.config).unwrap_or_else(|_| "{}".into());
    let row = SearchProviderRow {
        kind,
        enabled: req.enabled,
        is_default: req.is_default,
        config_json,
        created_at,
        updated_at: now,
    };
    store.upsert(&row).map_err(map_provider_err)?;
    let stored = store
        .get(kind)
        .map_err(map_provider_err)?
        .ok_or_else(|| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "post_upsert_lookup_failed",
            message: "row vanished after upsert".into(),
        })?;
    Ok(Json(row_to_view(stored)))
}

pub async fn delete_provider_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(kind): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let kind_enum = SearchProviderKind::parse(&kind).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_provider_kind",
        message: format!("unknown provider kind: {kind}"),
    })?;
    SearchProviderStore::new(&state.db)
        .delete(kind_enum)
        .map_err(map_provider_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct SetDefaultRequest {
    pub kind: String,
}

pub async fn set_default_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<SetDefaultRequest>,
) -> Result<Json<ProviderView>, ApiError> {
    require_controller(&state, &user)?;
    let kind = SearchProviderKind::parse(&req.kind).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_provider_kind",
        message: format!("unknown provider kind: {}", req.kind),
    })?;
    let now = chrono::Utc::now().timestamp();
    let store = SearchProviderStore::new(&state.db);
    let promoted = store.set_default(kind, now).map_err(map_provider_err)?;
    if !promoted {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "search_provider_not_found",
            message: format!(
                "no provider row for kind {}; upsert it first",
                kind.as_str()
            ),
        });
    }
    let stored = store
        .get(kind)
        .map_err(map_provider_err)?
        .expect("just promoted, must exist");
    Ok(Json(row_to_view(stored)))
}

pub async fn test_search_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(kind): Path<String>,
    Json(req): Json<TestSearchRequest>,
) -> Result<Json<TestSearchResponse>, ApiError> {
    require_controller(&state, &user)?;
    let kind_enum = SearchProviderKind::parse(&kind).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_provider_kind",
        message: format!("unknown provider kind: {kind}"),
    })?;
    let row = SearchProviderStore::new(&state.db)
        .get(kind_enum)
        .map_err(map_provider_err)?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "search_provider_not_found",
            message: format!("no provider row for kind {}", kind_enum.as_str()),
        })?;
    let provider = construct_from_row(&row);
    let started = std::time::Instant::now();
    let results = provider.search(&req.query, 5).await.map_err(|e| ApiError {
        status: StatusCode::BAD_GATEWAY,
        code: "search_failed",
        message: e.to_string(),
    })?;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(Json(TestSearchResponse {
        provider_id: provider.provider_id().to_owned(),
        results: results
            .into_iter()
            .map(|r| TestSearchHit {
                title: r.title,
                url: r.url,
                snippet: r.snippet,
            })
            .collect(),
        elapsed_ms,
    }))
}

pub fn settings_search_router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/admin/search/providers",
            get(list_providers_handler).post(upsert_provider_handler),
        )
        .route(
            "/api/admin/search/providers/{kind}",
            delete(delete_provider_handler),
        )
        .route(
            "/api/admin/search/providers/default",
            post(set_default_handler),
        )
        .route(
            "/api/admin/search/providers/{kind}/test",
            post(test_search_handler),
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
    async fn list_returns_seeded_duckduckgo_row() {
        let state = test_app_state();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/admin/search/providers")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let providers = v["providers"].as_array().unwrap();
        // Migration seeds DDG; assertion guards against
        // accidental re-ordering of the seed.
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["kind"], "duckduckgo");
        assert_eq!(providers[0]["is_default"], true);
    }

    #[tokio::test]
    async fn upsert_creates_searxng_row_with_config() {
        let state = test_app_state();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;

        let body = serde_json::to_vec(&serde_json::json!({
            "kind": "searxng",
            "enabled": true,
            "is_default": false,
            "config": {"base_url": "https://searx.example.com"},
        }))
        .unwrap();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/admin/search/providers")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["kind"], "searxng");
        assert_eq!(v["display_name"], "SearxNG (self-hosted)");
        assert_eq!(v["config"]["base_url"], "https://searx.example.com");
    }

    #[tokio::test]
    async fn set_default_promotes_row_and_demotes_others_via_trigger() {
        let state = test_app_state();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;

        // Add SearxNG (non-default).
        let upsert_body = serde_json::to_vec(&serde_json::json!({
            "kind": "searxng",
            "enabled": true,
            "is_default": false,
            "config": {"base_url": "https://searx.example.com"},
        }))
        .unwrap();
        app.clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/admin/search/providers")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(upsert_body))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Promote it.
        let promote_body = serde_json::to_vec(&serde_json::json!({"kind": "searxng"})).unwrap();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/admin/search/providers/default")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(promote_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["kind"], "searxng");
        assert_eq!(v["is_default"], true);

        // Verify only one default in the list.
        let list = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/admin/search/providers")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = body::to_bytes(list.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let providers = v["providers"].as_array().unwrap();
        let defaults = providers.iter().filter(|p| p["is_default"] == true).count();
        assert_eq!(defaults, 1);
    }

    #[tokio::test]
    async fn upsert_rejects_unknown_provider_kind() {
        let state = test_app_state();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let body = serde_json::to_vec(
            &serde_json::json!({"kind": "google", "enabled": true, "is_default": false, "config": {}}),
        )
        .unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/admin/search/providers")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn endpoints_require_controller_role() {
        let state = test_app_state();
        let app = build_router(state);
        // No auth token at all.
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/admin/search/providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
