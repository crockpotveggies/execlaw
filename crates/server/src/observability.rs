//! Observability admin routes (Phase 5).
//!
//! - `GET /api/admin/logs` — paginated `log_entries` rows with
//!   level / plugin_id / conversation_id / since filters.
//! - `GET /api/admin/eval/flags` — every eval-flag row, optionally
//!   filtered by label.
//!
//! Pure data feeds for the Phase-6 React UI. No HTML rendering;
//! no chart generation. The CLI replay command (`execlaw replay`)
//! is the operator-facing surface today.

use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Router, routing::get};
use execlaw_core::audit::AuditStore;
use execlaw_core::eval::EvalFlaggedStore;
use execlaw_core::logs::{LogLevel, LogStore};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Default)]
pub struct LogsQuery {
    /// One of `trace` / `debug` / `info` / `warn` / `error` (case
    /// insensitive). Omit to query every level.
    pub level: Option<String>,
    pub plugin_id: Option<String>,
    pub conversation_id: Option<String>,
    /// Inclusive lower bound on `ts_ms`.
    pub since_ms: Option<i64>,
    /// Hard cap; default 200, max 1000.
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct LogEntryView {
    pub ts_ms: i64,
    pub level: String,
    pub target: String,
    pub conversation_id: Option<String>,
    pub plugin_id: Option<String>,
    pub message: String,
    pub fields: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct LogsResponse {
    pub entries: Vec<LogEntryView>,
}

/// `GET /api/admin/logs`
#[utoipa::path(
    get,
    path = "/api/admin/logs",
    params(
        ("level" = Option<String>, Query, description = "trace|debug|info|warn|error"),
        ("plugin_id" = Option<String>, Query, description = "Filter to one plugin"),
        ("conversation_id" = Option<String>, Query, description = "Filter to one conversation"),
        ("since_ms" = Option<i64>, Query, description = "Inclusive lower bound on ts_ms"),
        ("limit" = Option<i64>, Query, description = "1..=1000, default 200"),
    ),
    responses(
        (status = 200, description = "Filtered log entries"),
        (status = 401, description = "Missing or invalid Authorization header"),
    ),
    security(("bearer_jwt" = [])),
    tag = "observability"
)]
pub async fn logs_handler(
    State(state): State<AppState>,
    _user: crate::auth_extract::AuthedUser,
    Query(q): Query<LogsQuery>,
) -> impl IntoResponse {
    let level = q
        .level
        .as_deref()
        .and_then(|s| match s.to_ascii_lowercase().as_str() {
            "trace" => Some(LogLevel::Trace),
            "debug" => Some(LogLevel::Debug),
            "info" => Some(LogLevel::Info),
            "warn" | "warning" => Some(LogLevel::Warn),
            "error" => Some(LogLevel::Error),
            _ => None,
        });
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let store = LogStore::new(&state.db);
    let rows = match store.query(
        level,
        q.plugin_id.as_deref(),
        q.conversation_id.as_deref(),
        q.since_ms,
        limit,
    ) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": {"code": "logs_query", "message": e.to_string()}
                })),
            )
                .into_response();
        }
    };
    let entries: Vec<LogEntryView> = rows
        .into_iter()
        .map(|r| LogEntryView {
            ts_ms: r.ts_ms,
            level: r.level.as_str().to_owned(),
            target: r.target,
            conversation_id: r.conversation_id,
            plugin_id: r.plugin_id,
            message: r.message,
            fields: r.fields_json.and_then(|b| serde_json::from_slice(&b).ok()),
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!(LogsResponse { entries })),
    )
        .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct FlagsQuery {
    pub label: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FlagView {
    pub id: i64,
    pub conversation_id: String,
    pub from_seq: i64,
    pub to_seq: i64,
    pub label: String,
    pub tags: Vec<String>,
    pub flagged_by: String,
    pub flagged_at: i64,
    pub notes: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FlagsResponse {
    pub flags: Vec<FlagView>,
}

/// `GET /api/admin/eval/flags`
#[utoipa::path(
    get,
    path = "/api/admin/eval/flags",
    params(
        ("label" = Option<String>, Query, description = "Filter to one eval label"),
        ("limit" = Option<i64>, Query, description = "1..=1000, default 200"),
    ),
    responses(
        (status = 200, description = "Eval-flag rows"),
        (status = 401, description = "Missing or invalid Authorization header"),
    ),
    security(("bearer_jwt" = [])),
    tag = "observability"
)]
pub async fn eval_flags_handler(
    State(state): State<AppState>,
    _user: crate::auth_extract::AuthedUser,
    Query(q): Query<FlagsQuery>,
) -> impl IntoResponse {
    let store = EvalFlaggedStore::new(&state.db);
    let rows = match q.label {
        Some(label) => store.list_by_label(&label),
        None => store.list_all(),
    };
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": {"code": "eval_query", "message": e.to_string()}
                })),
            )
                .into_response();
        }
    };
    let flags: Vec<FlagView> = rows
        .into_iter()
        .map(|r| FlagView {
            id: r.id.unwrap_or_default(),
            conversation_id: r.conversation_id.as_str().to_owned(),
            from_seq: r.from_seq,
            to_seq: r.to_seq,
            label: r.label,
            tags: r.tags,
            flagged_by: r.flagged_by,
            flagged_at: r.flagged_at,
            notes: r.notes,
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!(FlagsResponse { flags })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Config-audit feed
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct AuditQuery {
    /// Inclusive lower bound on `ts` (unix seconds).
    pub since_ts: Option<i64>,
    /// Hard cap; default 200, max 1000.
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct AuditEntryView {
    pub id: i64,
    pub ts: i64,
    pub actor: String,
    pub table_name: String,
    pub row_id: String,
    pub old_json: Option<serde_json::Value>,
    pub new_json: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct AuditResponse {
    pub entries: Vec<AuditEntryView>,
}

/// `GET /api/admin/audit` — recent rows from `config_audit`, newest
/// first. Empty until config-write routes start writing entries
/// (Phase 7 deployment editor onward).
#[utoipa::path(
    get,
    path = "/api/admin/audit",
    params(
        ("since_ts" = Option<i64>, Query, description = "Inclusive lower bound on ts (unix seconds)"),
        ("limit" = Option<i64>, Query, description = "1..=1000, default 200"),
    ),
    responses(
        (status = 200, description = "Config-mutation audit entries"),
        (status = 401, description = "Missing or invalid Authorization header"),
    ),
    security(("bearer_jwt" = [])),
    tag = "observability"
)]
pub async fn audit_handler(
    State(state): State<AppState>,
    _user: crate::auth_extract::AuthedUser,
    Query(q): Query<AuditQuery>,
) -> impl IntoResponse {
    let store = AuditStore::new(&state.db);
    let limit = q.limit.unwrap_or(200);
    let rows = match store.list(q.since_ts, limit) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("audit list: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "audit query failed"})),
            )
                .into_response();
        }
    };
    let entries: Vec<AuditEntryView> = rows
        .into_iter()
        .map(|r| AuditEntryView {
            id: r.id,
            ts: r.ts,
            actor: r.actor,
            table_name: r.table_name,
            row_id: r.row_id,
            old_json: r.old_json,
            new_json: r.new_json,
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!(AuditResponse { entries })),
    )
        .into_response()
}

pub fn observability_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/logs", get(logs_handler))
        .route("/api/admin/eval/flags", get(eval_flags_handler))
        .route("/api/admin/audit", get(audit_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::{self, Body};
    use axum::http::{Method, Request, header};
    use tower::ServiceExt;

    async fn setup_get_token(app: &axum::Router) -> String {
        let body = serde_json::to_vec(&serde_json::json!({
            "username": "tester",
            "admin_password": "hunter2-longer",
            "display_name": "Tester",
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

    async fn read_json(
        app: &axum::Router,
        token: Option<&str>,
        uri: &str,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(Method::GET).uri(uri);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let resp = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    #[tokio::test]
    async fn logs_endpoint_authn_then_empty_then_filtered() {
        let app = build_router(test_app_state());
        // 1. Auth gate.
        let (status, _) = read_json(&app, None, "/api/admin/logs").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let token = setup_get_token(&app).await;
        // 2. Fresh DB → empty entries.
        let (status, body) = read_json(&app, Some(&token), "/api/admin/logs").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["entries"].is_array());
        assert_eq!(body["entries"].as_array().unwrap().len(), 0);
        // 3. With a level filter, still 200, still empty.
        let (status, body) = read_json(&app, Some(&token), "/api/admin/logs?level=warn").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["entries"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn eval_flags_endpoint_authn_then_empty_then_filtered() {
        let app = build_router(test_app_state());
        let (status, _) = read_json(&app, None, "/api/admin/eval/flags").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let token = setup_get_token(&app).await;
        let (status, body) = read_json(&app, Some(&token), "/api/admin/eval/flags").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["flags"].is_array());
        assert_eq!(body["flags"].as_array().unwrap().len(), 0);
        // Label filter accepted; empty result.
        let (status, body) =
            read_json(&app, Some(&token), "/api/admin/eval/flags?label=regression").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["flags"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn audit_requires_auth() {
        let app = build_router(test_app_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/admin/audit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn audit_returns_empty_on_fresh_db() {
        let app = build_router(test_app_state());
        let token = setup_get_token(&app).await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/admin/audit")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["entries"].is_array());
        assert_eq!(v["entries"].as_array().unwrap().len(), 0);
    }

    /// Inserted rows surface in the GET feed.
    #[tokio::test]
    async fn audit_lists_inserted_rows() {
        let state = test_app_state();
        // Pre-populate via the store directly; the feed hits the same DB.
        AuditStore::new(&state.db)
            .insert(
                "controller-1",
                "config_alert_routing",
                "row-x",
                None,
                Some(&serde_json::json!({"k": "v"})),
            )
            .unwrap();
        let app = build_router(state);
        let token = setup_get_token(&app).await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/admin/audit")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let arr = v["entries"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["actor"], "controller-1");
        assert_eq!(arr[0]["table_name"], "config_alert_routing");
        assert_eq!(arr[0]["new_json"]["k"], "v");
    }
}
