//! Observability admin routes (Phase 5).
//!
//! - `GET /api/admin/logs` — reads the tracing file appender's
//!   `execlaw.jsonl.<DATE>` files from `ServerConfig.log_dir` and
//!   applies level / plugin_id / conversation_id / time-range filters.
//!   The `log_entries` SQLite table and `SqliteLogLayer` exist but
//!   were never plumbed onto the global subscriber (the layer needs
//!   the DB handle, which doesn't exist when `init_tracing` runs);
//!   reading the JSONL files directly is the simplest surface that
//!   actually shows operators their logs in Settings > Logs.
//! - `GET /api/admin/eval/flags` — every eval-flag row, optionally
//!   filtered by label.
//! - `GET /api/admin/audit` — recent `config_audit` rows.

use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Router, routing::get};
use execlaw_core::audit::AuditStore;
use execlaw_core::eval::EvalFlaggedStore;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Default)]
pub struct LogsQuery {
    /// One of `trace` / `debug` / `info` / `warn` / `error` (case
    /// insensitive). Omit to query every level.
    pub level: Option<String>,
    pub plugin_id: Option<String>,
    pub conversation_id: Option<String>,
    /// Inclusive lower bound on `ts_ms`.
    pub since_ms: Option<i64>,
    /// Inclusive upper bound on `ts_ms`.
    pub until_ms: Option<i64>,
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
        ("until_ms" = Option<i64>, Query, description = "Inclusive upper bound on ts_ms"),
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
    let limit = q.limit.unwrap_or(200).clamp(1, 1000) as usize;
    let level_floor = parse_level(q.level.as_deref());
    let Some(log_dir) = state.config.log_dir.as_deref() else {
        return (
            StatusCode::OK,
            Json(serde_json::json!(LogsResponse { entries: vec![] })),
        )
            .into_response();
    };
    let entries = match read_log_entries(
        log_dir,
        level_floor,
        q.plugin_id.as_deref(),
        q.conversation_id.as_deref(),
        q.since_ms,
        q.until_ms,
        limit,
    ) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": {"code": "logs_read", "message": e.to_string()}
                })),
            )
                .into_response();
        }
    };
    (
        StatusCode::OK,
        Json(serde_json::json!(LogsResponse { entries })),
    )
        .into_response()
}

/// trace=0 .. error=4. Used as a "minimum" floor — entries at or
/// above the selected level pass the filter.
fn level_rank(level: &str) -> u8 {
    match level.to_ascii_lowercase().as_str() {
        "trace" => 0,
        "debug" => 1,
        "info" => 2,
        "warn" | "warning" => 3,
        "error" => 4,
        _ => 2,
    }
}

fn parse_level(s: Option<&str>) -> Option<u8> {
    let s = s?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "trace" | "debug" | "info" | "warn" | "warning" | "error" => Some(level_rank(trimmed)),
        _ => None,
    }
}

/// Lists files matching `execlaw.jsonl[.<DATE>]` in `dir`, newest
/// first. tracing-appender's daily rotation produces names like
/// `execlaw.jsonl.2026-05-15`; sorting filenames descending gives
/// newest-first because the suffix is ISO date. Files without a
/// recognized date suffix (a hand-edited "execlaw.jsonl" left in the
/// dir, say) still get included, sorted to the end.
fn list_log_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|d| d.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("execlaw.jsonl"))
                    .unwrap_or(false)
            })
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    entries.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    Ok(entries)
}

#[allow(clippy::too_many_arguments)]
fn read_log_entries(
    dir: &Path,
    level_floor: Option<u8>,
    plugin_id: Option<&str>,
    conversation_id: Option<&str>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    limit: usize,
) -> std::io::Result<Vec<LogEntryView>> {
    let files = list_log_files(dir)?;
    let mut out: Vec<LogEntryView> = Vec::with_capacity(limit);
    // Walk newest file first; within a file, read forward and keep
    // a sliding window of the most recent `limit` matching entries.
    // Stop scanning earlier files once we have a full window AND the
    // earliest collected entry's timestamp predates the file's
    // contents — but that requires per-file metadata; the simpler
    // (and still cheap for daily files of typical size) approach is
    // to read until we hit a since_ms lower bound or run out.
    for path in files {
        let f = match fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        for line in BufReader::new(f).lines() {
            let Ok(line) = line else { continue };
            if line.trim().is_empty() {
                continue;
            }
            let Some(entry) = parse_jsonl_line(&line) else {
                continue;
            };
            if let Some(floor) = level_floor {
                if level_rank(&entry.level) < floor {
                    continue;
                }
            }
            if let Some(p) = plugin_id {
                if entry.plugin_id.as_deref() != Some(p) {
                    continue;
                }
            }
            if let Some(c) = conversation_id {
                if entry.conversation_id.as_deref() != Some(c) {
                    continue;
                }
            }
            if let Some(since) = since_ms {
                if entry.ts_ms < since {
                    continue;
                }
            }
            if let Some(until) = until_ms {
                if entry.ts_ms > until {
                    continue;
                }
            }
            out.push(entry);
        }
        // Heuristic early exit: once we've collected >= limit and the
        // OLDEST entry we have predates the start of the previous
        // (older) file, additional files can only contribute older
        // rows that we'd drop anyway. We can't cheaply prove that
        // without parsing file names, so just keep reading — daily
        // files are bounded in size, and the UI caps at 1000 anyway.
        if out.len() >= limit * 4 {
            break;
        }
    }
    out.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms));
    out.truncate(limit);
    Ok(out)
}

fn parse_jsonl_line(line: &str) -> Option<LogEntryView> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let obj = v.as_object()?;
    let ts_ms = obj
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis())?;
    let level = obj
        .get("level")
        .and_then(|l| l.as_str())
        .unwrap_or("INFO")
        .to_owned();
    let target = obj
        .get("target")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_owned();
    // tracing-appender's JSON format nests user fields (and the
    // event's format-string `message`) under `fields`. Pull
    // `message` / `conversation_id` / `plugin_id` out, leave the
    // remainder in `fields` for the UI's expandable rendering later.
    let mut fields_obj = obj
        .get("fields")
        .and_then(|f| f.as_object())
        .cloned()
        .unwrap_or_default();
    let message = fields_obj
        .remove("message")
        .and_then(|m| m.as_str().map(|s| s.to_owned()))
        .unwrap_or_else(|| target.clone());
    let conversation_id = fields_obj
        .remove("conversation_id")
        .and_then(|m| m.as_str().map(|s| s.to_owned()));
    let plugin_id = fields_obj
        .remove("plugin_id")
        .and_then(|m| m.as_str().map(|s| s.to_owned()));
    let fields = if fields_obj.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(fields_obj))
    };
    Some(LogEntryView {
        ts_ms,
        level,
        target,
        conversation_id,
        plugin_id,
        message,
        fields,
    })
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
        // 2. `log_dir = None` in default config → empty entries.
        let (status, body) = read_json(&app, Some(&token), "/api/admin/logs").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["entries"].is_array());
        assert_eq!(body["entries"].as_array().unwrap().len(), 0);
        // 3. With a level filter, still 200, still empty.
        let (status, body) = read_json(&app, Some(&token), "/api/admin/logs?level=warn").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["entries"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn parse_jsonl_line_extracts_known_fields() {
        let line = r#"{"timestamp":"2026-05-15T10:30:00.123Z","level":"INFO","fields":{"message":"hello","conversation_id":"conv-1","plugin_id":"weather","extra":42},"target":"execlaw_server::chats"}"#;
        let entry = parse_jsonl_line(line).expect("parses");
        assert_eq!(entry.level, "INFO");
        assert_eq!(entry.target, "execlaw_server::chats");
        assert_eq!(entry.message, "hello");
        assert_eq!(entry.conversation_id.as_deref(), Some("conv-1"));
        assert_eq!(entry.plugin_id.as_deref(), Some("weather"));
        assert_eq!(entry.fields.as_ref().unwrap()["extra"], 42);
        let expected = chrono::DateTime::parse_from_rfc3339("2026-05-15T10:30:00.123Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(entry.ts_ms, expected);
    }

    #[test]
    fn parse_jsonl_line_falls_back_to_target_when_no_message() {
        let line =
            r#"{"timestamp":"2026-05-15T10:30:00Z","level":"WARN","fields":{},"target":"some::module"}"#;
        let entry = parse_jsonl_line(line).expect("parses");
        assert_eq!(entry.message, "some::module");
        assert!(entry.fields.is_none());
    }

    #[test]
    fn parse_jsonl_line_returns_none_on_garbage() {
        assert!(parse_jsonl_line("not json").is_none());
        // Missing timestamp = unusable.
        assert!(parse_jsonl_line(r#"{"level":"INFO"}"#).is_none());
    }

    #[test]
    fn read_log_entries_filters_and_sorts_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path_day1 = dir.path().join("execlaw.jsonl.2026-05-14");
        let path_day2 = dir.path().join("execlaw.jsonl.2026-05-15");
        std::fs::write(
            &path_day1,
            "\
{\"timestamp\":\"2026-05-14T09:00:00Z\",\"level\":\"INFO\",\"fields\":{\"message\":\"a\"},\"target\":\"t\"}
{\"timestamp\":\"2026-05-14T09:01:00Z\",\"level\":\"DEBUG\",\"fields\":{\"message\":\"b\"},\"target\":\"t\"}
",
        )
        .unwrap();
        std::fs::write(
            &path_day2,
            "\
{\"timestamp\":\"2026-05-15T10:00:00Z\",\"level\":\"ERROR\",\"fields\":{\"message\":\"c\",\"plugin_id\":\"weather\"},\"target\":\"t\"}
{\"timestamp\":\"2026-05-15T10:05:00Z\",\"level\":\"INFO\",\"fields\":{\"message\":\"d\"},\"target\":\"t\"}
",
        )
        .unwrap();
        // No filter → all 4, newest first: d (10:05 day2), c (10:00 day2),
        // b (09:01 day1), a (09:00 day1).
        let v = read_log_entries(dir.path(), None, None, None, None, None, 100).unwrap();
        assert_eq!(v.len(), 4);
        assert_eq!(v[0].message, "d");
        assert_eq!(v[1].message, "c");
        assert_eq!(v[2].message, "b");
        assert_eq!(v[3].message, "a");
        // Level floor = warn → only the error.
        let v = read_log_entries(dir.path(), Some(3), None, None, None, None, 100).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].message, "c");
        // Plugin filter.
        let v = read_log_entries(dir.path(), None, Some("weather"), None, None, None, 100).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].plugin_id.as_deref(), Some("weather"));
        // since_ms cuts off day-1 entries.
        let cutoff = chrono::DateTime::parse_from_rfc3339("2026-05-15T00:00:00Z")
            .unwrap()
            .timestamp_millis();
        let v = read_log_entries(dir.path(), None, None, None, Some(cutoff), None, 100).unwrap();
        assert_eq!(v.len(), 2);
        // limit truncates after sort.
        let v = read_log_entries(dir.path(), None, None, None, None, None, 2).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].message, "d");
        assert_eq!(v[1].message, "c");
    }

    #[test]
    fn read_log_entries_missing_dir_is_empty_not_error() {
        let v = read_log_entries(
            std::path::Path::new("/definitely/does/not/exist"),
            None,
            None,
            None,
            None,
            None,
            100,
        )
        .unwrap();
        assert!(v.is_empty());
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
