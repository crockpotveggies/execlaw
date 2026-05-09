//! Settings → Alerts — operator-facing alert list with ack/resolve.
//!
//! Backs MIGRATION_PLAN §10. Storage + dedup are in `crates/core`; this
//! module just exposes the operator's read + state-transition surface
//! over HTTP. The alert pipeline that ACTUALLY produces alerts (rate
//! limits, plugin failures, OAuth expiry) lives in feature crates and
//! writes through `AlertStore::insert_firing` directly — those rows
//! show up on the page automatically.
//!
//! Routes:
//!   * `GET  /api/admin/alerts?status=Firing&limit=200`
//!     — list alerts with optional status filter + cap. Default
//!     status filter is "all"; the SPA defaults to Firing for the
//!     dropdown view.
//!   * `GET  /api/admin/alerts/count` — quick `firing` count for the
//!     SPA badge. Cheaper than listing when the operator just needs
//!     to know "is there anything to look at?"
//!   * `POST /api/admin/alerts/{id}/ack`     (Controller-only)
//!   * `POST /api/admin/alerts/{id}/resolve` (Controller-only)

use crate::auth_extract::AuthedUser;
use crate::events::UiEvent;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use execlaw_core::AlertId;
use execlaw_core::alerts::{AlertRow, AlertStatus, AlertStore, Severity};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct AlertView {
    pub id: String,
    pub fingerprint: String,
    pub severity: String,
    pub source: String,
    pub title: String,
    pub detail: Option<String>,
    pub status: String,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub occurrence_count: i64,
    pub resolved_at: Option<i64>,
    pub resolved_by: Option<String>,
    pub ack_at: Option<i64>,
    pub ack_by: Option<String>,
    pub snooze_until: Option<i64>,
    pub incident_id: Option<String>,
}

impl From<&AlertRow> for AlertView {
    fn from(r: &AlertRow) -> Self {
        Self {
            id: r.id.as_str().to_owned(),
            fingerprint: r.fingerprint.clone(),
            severity: r.severity.as_str().to_owned(),
            source: r.source.clone(),
            title: r.title.clone(),
            detail: r.detail.clone(),
            status: r.status.as_str().to_owned(),
            first_seen_at: r.first_seen_at,
            last_seen_at: r.last_seen_at,
            occurrence_count: r.occurrence_count,
            resolved_at: r.resolved_at,
            resolved_by: r.resolved_by.clone(),
            ack_at: r.ack_at,
            ack_by: r.ack_by.clone(),
            snooze_until: r.snooze_until,
            incident_id: r.incident_id.as_ref().map(|i| i.as_str().to_owned()),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AlertListResponse {
    pub alerts: Vec<AlertView>,
    /// Always-present count of `Firing` alerts, regardless of the
    /// `status` filter — saves the SPA a second round-trip when it
    /// wants to update the badge after listing.
    pub firing_count: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AlertCountResponse {
    pub firing_count: i64,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Comma-separated subset of `Firing,Acked,Resolved,Snoozed`.
    /// Omitting the field returns all statuses; passing an empty
    /// string is treated the same as omitting it.
    pub status: Option<String>,
    /// Cap on the row count. Defaults to 200 if omitted; max 1000 to
    /// keep payloads sane.
    pub limit: Option<u32>,
}

#[utoipa::path(
    get,
    path = "/api/admin/alerts",
    params(
        ("status" = Option<String>, Query, description = "Comma-separated statuses (Firing,Acked,Resolved,Snoozed)"),
        ("limit"  = Option<u32>,    Query, description = "Cap on the row count, default 200, max 1000"),
    ),
    responses((status = 200, description = "Alerts + firing count", body = AlertListResponse)),
    security(("bearer_jwt" = [])),
    tag = "alerts"
)]
pub async fn list_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
    Query(q): Query<ListQuery>,
) -> Result<Json<AlertListResponse>, ApiError> {
    let store = AlertStore::new(&state.db);

    let limit = q.limit.unwrap_or(200).min(1000);

    let status_set: Option<Vec<AlertStatus>> = match q.status.as_deref() {
        None => None,
        Some(s) if s.trim().is_empty() => None,
        Some(s) => {
            let parsed: Vec<AlertStatus> = s
                .split(',')
                .map(|t| t.trim())
                .filter(|t| !t.is_empty())
                .filter_map(AlertStatus::parse)
                .collect();
            if parsed.is_empty() {
                return Err(ApiError {
                    status: StatusCode::BAD_REQUEST,
                    code: "invalid_status",
                    message: format!("no recognised statuses in '{s}'"),
                });
            }
            Some(parsed)
        }
    };

    let rows = store
        .list(status_set.as_deref(), Some(limit))
        .map_err(db_err)?;
    let firing_count = store.count_firing().map_err(db_err)?;

    Ok(Json(AlertListResponse {
        alerts: rows.iter().map(AlertView::from).collect(),
        firing_count,
    }))
}

#[utoipa::path(
    get,
    path = "/api/admin/alerts/count",
    responses((status = 200, description = "Firing-alert count for the SPA badge", body = AlertCountResponse)),
    security(("bearer_jwt" = [])),
    tag = "alerts"
)]
pub async fn count_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<AlertCountResponse>, ApiError> {
    let store = AlertStore::new(&state.db);
    Ok(Json(AlertCountResponse {
        firing_count: store.count_firing().map_err(db_err)?,
    }))
}

#[utoipa::path(
    post,
    path = "/api/admin/alerts/{id}/ack",
    params(("id" = String, Path, description = "Alert id")),
    responses(
        (status = 200, description = "Acked"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "Alert id not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "alerts"
)]
pub async fn ack_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let store = AlertStore::new(&state.db);
    let alert_id = AlertId::from(id);
    if store.get(&alert_id).map_err(db_err)?.is_none() {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "alert_not_found",
            message: format!("alert '{}' does not exist", alert_id.as_str()),
        });
    }
    let now = chrono::Utc::now().timestamp();
    store.ack(&alert_id, &user.user_id, now).map_err(db_err)?;
    // §10.8: push so the badge updates without waiting for the next
    // poll. Acked counts as "no longer firing" from the badge's
    // perspective; the SPA decrements on this event.
    state.events.publish(UiEvent::AlertResolved {
        alert_id: alert_id.as_str().to_owned(),
    });
    Ok(StatusCode::OK)
}

#[utoipa::path(
    post,
    path = "/api/admin/alerts/{id}/resolve",
    params(("id" = String, Path, description = "Alert id")),
    responses(
        (status = 200, description = "Resolved"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "Alert id not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "alerts"
)]
pub async fn resolve_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let store = AlertStore::new(&state.db);
    let alert_id = AlertId::from(id);
    if store.get(&alert_id).map_err(db_err)?.is_none() {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "alert_not_found",
            message: format!("alert '{}' does not exist", alert_id.as_str()),
        });
    }
    let now = chrono::Utc::now().timestamp();
    store
        .resolve(&alert_id, &user.user_id, now)
        .map_err(db_err)?;
    state.events.publish(UiEvent::AlertResolved {
        alert_id: alert_id.as_str().to_owned(),
    });
    Ok(StatusCode::OK)
}

fn db_err(e: execlaw_core::DbError) -> ApiError {
    ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "db_error",
        message: e.to_string(),
    }
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
            message: "only a Controller can change alert state".into(),
        }),
    }
}

pub fn alerts_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/alerts", get(list_handler))
        .route("/api/admin/alerts/count", get(count_handler))
        .route("/api/admin/alerts/{id}/ack", post(ack_handler))
        .route("/api/admin/alerts/{id}/resolve", post(resolve_handler))
}

// Severity is re-exported below so the docs.rs schemas registry can
// pull it in without a deeper core import dance.
pub fn _severity_marker(_s: Severity) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::{self, Body};
    use axum::http::{Method, Request, header};
    use execlaw_core::alerts::{AlertRow, AlertStatus, AlertStore, Severity};
    use execlaw_core::ids::AlertId;
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

    fn mk_alert(state: &crate::state::AppState, fingerprint: &str) -> AlertId {
        let row = AlertRow {
            id: AlertId::new(),
            fingerprint: fingerprint.into(),
            severity: Severity::Error,
            source: "core.test".into(),
            title: format!("alert {fingerprint}"),
            detail: None,
            context_json: None,
            status: AlertStatus::Firing,
            first_seen_at: 100,
            last_seen_at: 100,
            occurrence_count: 1,
            resolved_at: None,
            resolved_by: None,
            ack_at: None,
            ack_by: None,
            snooze_until: None,
            incident_id: None,
            actions_json: None,
        };
        let id = row.id.clone();
        AlertStore::new(&state.db).insert_firing(&row).unwrap();
        id
    }

    #[tokio::test]
    async fn list_returns_seeded_rows_with_firing_count() {
        let state = test_app_state();
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;
        let _a = mk_alert(&state, "fp-1");
        let _b = mk_alert(&state, "fp-2");

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/alerts")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["alerts"].as_array().unwrap().len(), 2);
        assert_eq!(v["firing_count"], 2);
    }

    #[tokio::test]
    async fn list_filters_by_status_query() {
        let state = test_app_state();
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;
        let a = mk_alert(&state, "fp-1");
        let _b = mk_alert(&state, "fp-2");
        // Ack the first one so the filter has a clear dividing line.
        AlertStore::new(&state.db).ack(&a, "ctrl", 200).unwrap();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/alerts?status=Firing")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let arr = v["alerts"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["fingerprint"], "fp-2");
        assert_eq!(v["firing_count"], 1);
    }

    #[tokio::test]
    async fn count_route_returns_firing_only() {
        let state = test_app_state();
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;
        let a = mk_alert(&state, "fp-1");
        let _b = mk_alert(&state, "fp-2");
        AlertStore::new(&state.db).resolve(&a, "ctrl", 200).unwrap();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/alerts/count")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["firing_count"], 1);
    }

    #[tokio::test]
    async fn ack_then_resolve_updates_status() {
        let state = test_app_state();
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;
        let id = mk_alert(&state, "fp-rt");

        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/admin/alerts/{}/ack", id.as_str()))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/admin/alerts/{}/resolve", id.as_str()))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let row = AlertStore::new(&state.db).get(&id).unwrap().unwrap();
        assert_eq!(row.status, AlertStatus::Resolved);
        assert!(row.ack_at.is_some());
        assert!(row.resolved_at.is_some());
    }

    #[tokio::test]
    async fn ack_unknown_id_404s() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/alerts/does-not-exist/ack")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn ack_publishes_alert_resolved_event() {
        // §10.8: ack/resolve must push an `alert_resolved` event so the
        // SPA can drop the badge live without waiting for the 60s poll.
        let state = test_app_state();
        let mut rx = state.events.subscribe();
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;
        let id = mk_alert(&state, "fp-ws");

        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/admin/alerts/{}/ack", id.as_str()))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Drain events until we see AlertResolved (the bus may emit
        // unrelated test events first; we only require ours appears).
        let mut found = false;
        for _ in 0..16 {
            match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
                Ok(Ok(crate::events::UiEvent::AlertResolved { alert_id })) => {
                    if alert_id == id.as_str() {
                        found = true;
                        break;
                    }
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(found, "ack should publish AlertResolved");
    }

    #[tokio::test]
    async fn list_with_unknown_status_400s() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/alerts?status=NotARealStatus")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
