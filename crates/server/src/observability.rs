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
    ),
    tag = "observability"
)]
pub async fn logs_handler(
    State(state): State<AppState>,
    Query(q): Query<LogsQuery>,
) -> impl IntoResponse {
    let level = q.level.as_deref().and_then(|s| match s.to_ascii_lowercase().as_str() {
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
            fields: r
                .fields_json
                .and_then(|b| serde_json::from_slice(&b).ok()),
        })
        .collect();
    (StatusCode::OK, Json(serde_json::json!(LogsResponse { entries })))
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
    ),
    tag = "observability"
)]
pub async fn eval_flags_handler(
    State(state): State<AppState>,
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
    (StatusCode::OK, Json(serde_json::json!(FlagsResponse { flags })))
        .into_response()
}

pub fn observability_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/logs", get(logs_handler))
        .route("/api/admin/eval/flags", get(eval_flags_handler))
}
