//! Settings → Trust policy — operator-editable trust ladder rules
//! (Phase 9.2, MIGRATION_PLAN §2.6 + §850).
//!
//! Single resource, two methods:
//!   * `GET /api/admin/trust-policy` — Controller + Operator can read
//!     the resolved policy snapshot (every field populated, with
//!     defaults filled in for unset keys).
//!   * `PUT /api/admin/trust-policy` — Controller-only. Validates the
//!     full payload and writes atomically; partial updates are
//!     rejected so the operator can't accidentally end up with a
//!     half-applied policy after a network glitch.

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use execlaw_core::audit::AuditStore;
use execlaw_core::trust_policy::{
    AutoTrustClass, MinTrustHint, MixedTrustPolicy, TrustPolicy, TrustPolicyError, TrustPolicyStore,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TrustPolicyView {
    pub auto_trust_contacts: bool,
    /// One of "Contact" | "Colleague" | "Organization".
    pub min_trust_hint_for_auto_trust: String,
    /// One of "KnownLimited" | "KnownTrusted". Trust class an
    /// auto-admitted (plugin-vouched) sender enters at.
    pub auto_trust_class: String,
    /// One of "min_wins" (only value today; reserved for future).
    pub mixed_trust_policy: String,
    pub identity_plugin_order: Vec<String>,
    /// Duration string, e.g. "7d", "12h", "30m".
    pub delegated_trust_default_ttl: String,
}

impl From<&TrustPolicy> for TrustPolicyView {
    fn from(p: &TrustPolicy) -> Self {
        Self {
            auto_trust_contacts: p.auto_trust_contacts,
            min_trust_hint_for_auto_trust: p.min_trust_hint_for_auto_trust.as_str().to_owned(),
            auto_trust_class: p.auto_trust_class.as_str().to_owned(),
            mixed_trust_policy: p.mixed_trust_policy.as_str().to_owned(),
            identity_plugin_order: p.identity_plugin_order.clone(),
            delegated_trust_default_ttl: p.delegated_trust_default_ttl.clone(),
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateTrustPolicyRequest {
    pub auto_trust_contacts: bool,
    pub min_trust_hint_for_auto_trust: String,
    /// Defaults to "KnownLimited" if omitted by an older client so
    /// existing SPA versions don't break post-upgrade.
    #[serde(default = "default_auto_trust_class")]
    pub auto_trust_class: String,
    pub mixed_trust_policy: String,
    pub identity_plugin_order: Vec<String>,
    pub delegated_trust_default_ttl: String,
}

fn default_auto_trust_class() -> String {
    "KnownLimited".to_owned()
}

impl From<TrustPolicyError> for ApiError {
    fn from(err: TrustPolicyError) -> Self {
        match err {
            TrustPolicyError::Db(e) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "db_error",
                message: e.to_string(),
            },
            TrustPolicyError::Invalid(msg) => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_trust_policy",
                message: msg,
            },
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/trust-policy",
    responses((status = 200, description = "Resolved trust policy snapshot", body = TrustPolicyView)),
    security(("bearer_jwt" = [])),
    tag = "trust-policy"
)]
pub async fn get_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<TrustPolicyView>, ApiError> {
    let store = TrustPolicyStore::new(&state.db);
    let p = store.read().map_err(ApiError::from)?;
    Ok(Json((&p).into()))
}

#[utoipa::path(
    put,
    path = "/api/admin/trust-policy",
    request_body = UpdateTrustPolicyRequest,
    responses(
        (status = 200, description = "Updated", body = TrustPolicyView),
        (status = 400, description = "Validation failed"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "trust-policy"
)]
pub async fn put_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<UpdateTrustPolicyRequest>,
) -> Result<Json<TrustPolicyView>, ApiError> {
    require_controller(&state, &user)?;

    let min_hint =
        MinTrustHint::parse(&req.min_trust_hint_for_auto_trust).ok_or_else(|| ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_min_trust_hint",
            message: format!(
                "min_trust_hint_for_auto_trust must be Contact|Colleague|Organization, got '{}'",
                req.min_trust_hint_for_auto_trust
            ),
        })?;
    let auto_class = AutoTrustClass::parse(&req.auto_trust_class).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_auto_trust_class",
        message: format!(
            "auto_trust_class must be KnownLimited|KnownTrusted, got '{}'",
            req.auto_trust_class
        ),
    })?;
    let mixed = MixedTrustPolicy::parse(&req.mixed_trust_policy).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_mixed_trust_policy",
        message: format!(
            "mixed_trust_policy must be 'min_wins' (today), got '{}'",
            req.mixed_trust_policy
        ),
    })?;

    let next = TrustPolicy {
        auto_trust_contacts: req.auto_trust_contacts,
        min_trust_hint_for_auto_trust: min_hint,
        auto_trust_class: auto_class,
        mixed_trust_policy: mixed,
        identity_plugin_order: req.identity_plugin_order.clone(),
        delegated_trust_default_ttl: req.delegated_trust_default_ttl.clone(),
    };

    let store = TrustPolicyStore::new(&state.db);
    let prior = store.read().map_err(ApiError::from)?;
    store.write(&next).map_err(ApiError::from)?;

    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "config_trust_policy",
        "policy",
        Some(&serde_json::json!({
            "auto_trust_contacts": prior.auto_trust_contacts,
            "min_trust_hint_for_auto_trust": prior.min_trust_hint_for_auto_trust.as_str(),
            "auto_trust_class": prior.auto_trust_class.as_str(),
            "mixed_trust_policy": prior.mixed_trust_policy.as_str(),
            "delegated_trust_default_ttl": prior.delegated_trust_default_ttl,
        })),
        Some(&serde_json::json!({
            "auto_trust_contacts": next.auto_trust_contacts,
            "min_trust_hint_for_auto_trust": next.min_trust_hint_for_auto_trust.as_str(),
            "auto_trust_class": next.auto_trust_class.as_str(),
            "mixed_trust_policy": next.mixed_trust_policy.as_str(),
            "delegated_trust_default_ttl": next.delegated_trust_default_ttl,
        })),
    );

    Ok(Json((&next).into()))
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
            message: "only a Controller can change trust policy".into(),
        }),
    }
}

pub fn trust_policy_router() -> Router<AppState> {
    Router::new().route("/api/admin/trust-policy", get(get_handler).put(put_handler))
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
    async fn get_returns_documented_defaults_on_fresh_db() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/trust-policy")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["auto_trust_contacts"], true);
        assert_eq!(v["min_trust_hint_for_auto_trust"], "Contact");
        assert_eq!(v["auto_trust_class"], "KnownLimited");
        assert_eq!(v["mixed_trust_policy"], "min_wins");
        assert_eq!(v["delegated_trust_default_ttl"], "7d");
    }

    #[tokio::test]
    async fn put_then_get_round_trips_full_policy() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let body = serde_json::json!({
            "auto_trust_contacts": false,
            "min_trust_hint_for_auto_trust": "Colleague",
            "auto_trust_class": "KnownTrusted",
            "mixed_trust_policy": "min_wins",
            "identity_plugin_order": ["plugin-a", "plugin-b"],
            "delegated_trust_default_ttl": "12h",
        });
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/trust-policy")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/trust-policy")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["auto_trust_contacts"], false);
        assert_eq!(v["min_trust_hint_for_auto_trust"], "Colleague");
        assert_eq!(v["auto_trust_class"], "KnownTrusted");
        assert_eq!(v["delegated_trust_default_ttl"], "12h");
        assert_eq!(v["identity_plugin_order"][0], "plugin-a");
    }

    #[tokio::test]
    async fn put_rejects_unknown_min_trust_hint() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let body = serde_json::json!({
            "auto_trust_contacts": true,
            "min_trust_hint_for_auto_trust": "BestFriend",
            "mixed_trust_policy": "min_wins",
            "identity_plugin_order": [],
            "delegated_trust_default_ttl": "7d",
        });
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/trust-policy")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn put_rejects_bad_ttl_format() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let body = serde_json::json!({
            "auto_trust_contacts": true,
            "min_trust_hint_for_auto_trust": "Contact",
            "mixed_trust_policy": "min_wins",
            "identity_plugin_order": [],
            "delegated_trust_default_ttl": "yesterday",
        });
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/trust-policy")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
