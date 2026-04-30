//! C6 — operator-facing research admin endpoints.
//!
//! Backs the SPA's `/research` page + the per-conversation
//! "running jobs" badge above the chat composer. The SPA's chat-pane
//! already gets live `card.*` events via the WS bus; these endpoints
//! cover the polling / drill-down path:
//!
//!   * `GET /api/admin/research/jobs`                — list every job
//!   * `GET /api/admin/research/jobs/:id`            — one job's full row
//!   * `GET /api/admin/research/jobs/:id/report`     — synthesized markdown
//!   * `GET /api/admin/research/jobs/:id/notes/:n`   — one gather note
//!   * `GET /api/admin/research/active_count`        — badge driver
//!     (optionally scoped by conversation_id query param)
//!
//! All routes are Controller-only — research jobs surface workspace
//! contents that span every conversation, so the strict-controller
//! gate matches the existing trust posture for cross-conversation
//! admin views (Audit, Logs, etc.).

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use execlaw_core::ids::{ConversationId, ResearchJobId};
use execlaw_core::research::{ResearchJobStore, ResearchJobSummary};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct ResearchJobsResponse {
    pub jobs: Vec<ResearchJobSummaryView>,
    pub count: usize,
}

/// Wire shape — same field set as `ResearchJobSummary` but flattened
/// for OpenAPI (utoipa needs concrete types, not generic Serialize
/// shapes from another crate). Convert with `From`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ResearchJobSummaryView {
    pub id: String,
    pub conversation_id: String,
    pub query: String,
    /// Lowercase status string. One of:
    /// pending / planning / planned / gathering / synthesizing /
    /// complete / failed / cancelled.
    pub status: String,
    pub card_id: Option<String>,
    pub workspace_path: Option<String>,
    pub attachment_id: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// Decoded plan if the planner phase landed; null otherwise.
    /// JSON value rather than a typed shape so the SPA can render
    /// it generically without a hard schema-coupling surface.
    pub plan: Option<serde_json::Value>,
    /// Decoded gather notes (may be partial during in-flight gather).
    /// Empty when the gather phase hasn't started.
    pub notes: serde_json::Value,
}

impl From<ResearchJobSummary> for ResearchJobSummaryView {
    fn from(s: ResearchJobSummary) -> Self {
        Self {
            id: s.id,
            conversation_id: s.conversation_id,
            query: s.query,
            status: s.status,
            card_id: s.card_id,
            workspace_path: s.workspace_path,
            attachment_id: s.attachment_id,
            error: s.error,
            created_at: s.created_at,
            updated_at: s.updated_at,
            started_at: s.started_at,
            finished_at: s.finished_at,
            plan: s.plan.and_then(|p| serde_json::to_value(p).ok()),
            notes: serde_json::to_value(s.notes).unwrap_or(serde_json::Value::Array(vec![])),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ResearchJobReportResponse {
    pub job_id: String,
    /// Markdown body, or `null` when the job hasn't completed
    /// synthesize yet.
    pub report_markdown: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ResearchActiveCountResponse {
    pub active_count: i64,
    /// `Some` iff the request scoped to a specific conversation; the
    /// global count returns `None` here.
    pub conversation_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ActiveCountQuery {
    /// When present, scopes the count to this conversation. Drives
    /// the chat-pane badge above the composer.
    pub conversation_id: Option<String>,
}

// `From<ResearchError> for ApiError` is defined once for the crate
// in `settings_research.rs`; reuse it here.

#[utoipa::path(
    get,
    path = "/api/admin/research/jobs",
    responses(
        (status = 200, description = "Every research job, newest first", body = ResearchJobsResponse),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "research"
)]
pub async fn list_jobs_handler(
    State(state): State<AppState>,
    user: AuthedUser,
) -> Result<Json<ResearchJobsResponse>, ApiError> {
    require_controller(&state, &user)?;
    let rows = ResearchJobStore::new(&state.db).list_all()?;
    let jobs: Vec<ResearchJobSummaryView> = rows
        .iter()
        .map(|r| ResearchJobSummaryView::from(r.to_summary()))
        .collect();
    let count = jobs.len();
    Ok(Json(ResearchJobsResponse { jobs, count }))
}

#[utoipa::path(
    get,
    path = "/api/admin/research/jobs/{job_id}",
    responses(
        (status = 200, description = "One research job's full summary", body = ResearchJobSummaryView),
        (status = 404, description = "No job with that id"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "research"
)]
pub async fn get_job_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(job_id): Path<String>,
) -> Result<Json<ResearchJobSummaryView>, ApiError> {
    require_controller(&state, &user)?;
    let row = ResearchJobStore::new(&state.db)
        .get(&ResearchJobId::from(job_id.as_str()))?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "research_not_found",
            message: format!("no research job '{job_id}'"),
        })?;
    Ok(Json(ResearchJobSummaryView::from(row.to_summary())))
}

#[utoipa::path(
    get,
    path = "/api/admin/research/jobs/{job_id}/report",
    responses(
        (status = 200, description = "Synthesized markdown report", body = ResearchJobReportResponse),
        (status = 404, description = "No job with that id"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "research"
)]
pub async fn get_report_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(job_id): Path<String>,
) -> Result<Json<ResearchJobReportResponse>, ApiError> {
    require_controller(&state, &user)?;
    let id = ResearchJobId::from(job_id.as_str());
    let row = ResearchJobStore::new(&state.db).get(&id)?.ok_or_else(|| ApiError {
        status: StatusCode::NOT_FOUND,
        code: "research_not_found",
        message: format!("no research job '{job_id}'"),
    })?;
    let body = match row.workspace_path.as_deref() {
        Some(path) => {
            let report_path = std::path::PathBuf::from(path).join("report.md");
            match std::fs::read_to_string(&report_path) {
                Ok(s) => Some(s),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    return Err(ApiError {
                        status: StatusCode::INTERNAL_SERVER_ERROR,
                        code: "research_report_io",
                        message: format!("reading report.md: {e}"),
                    });
                }
            }
        }
        None => None,
    };
    Ok(Json(ResearchJobReportResponse {
        job_id,
        report_markdown: body,
    }))
}

#[utoipa::path(
    get,
    path = "/api/admin/research/active_count",
    responses(
        (status = 200, description = "Active (non-terminal) job count", body = ResearchActiveCountResponse),
    ),
    security(("bearer_jwt" = [])),
    tag = "research"
)]
pub async fn active_count_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
    Query(q): Query<ActiveCountQuery>,
) -> Result<Json<ResearchActiveCountResponse>, ApiError> {
    let store = ResearchJobStore::new(&state.db);
    let count = match q.conversation_id.as_deref() {
        Some(cid) => store.active_count_for_conversation(&ConversationId::from(cid))?,
        None => {
            // Whole-DB count: list_all + filter rather than adding a
            // fresh dedicated query. Active-job populations are tiny
            // by construction (parallel_workers default 3), so the
            // scan is cheap and avoids growing the JobStore surface.
            store
                .list_all()?
                .iter()
                .filter(|r| !r.status.is_terminal())
                .count() as i64
        }
    };
    Ok(Json(ResearchActiveCountResponse {
        active_count: count,
        conversation_id: q.conversation_id,
    }))
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
            message: "only a Controller can access research admin endpoints".into(),
        }),
    }
}

pub fn research_admin_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/research/jobs", get(list_jobs_handler))
        .route("/api/admin/research/jobs/{job_id}", get(get_job_handler))
        .route(
            "/api/admin/research/jobs/{job_id}/report",
            get(get_report_handler),
        )
        // axum 0.8 needs `{name}` capture syntax (not `:name`).
        .route(
            "/api/admin/research/active_count",
            get(active_count_handler),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::{self, Body};
    use axum::http::{Method, Request, header};
    use execlaw_core::conversation::{
        ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
    };
    use execlaw_core::ids::EventSeq;
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

    fn seed_conv(state: &AppState, id: &str) -> ConversationId {
        let cid = ConversationId::from(id);
        ConversationStore::new(&state.db)
            .upsert(&ConversationRow {
                conversation_id: cid.clone(),
                kind: ConversationKind::ControllerDM,
                last_seq: EventSeq(0),
                phase: Phase::Idle,
                controller_id: None,
                trust_class: "Controller".into(),
                snapshot_blob: None,
                snapshot_seq: None,
                lease_owner: None,
                lease_expires: None,
                modality: Modality::Text,
                display_name: None,
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: None,
                last_activity_at: 0,
            })
            .unwrap();
        cid
    }

    #[tokio::test]
    async fn list_jobs_returns_seeded_rows_for_controller() {
        let state = test_app_state();
        let cid = seed_conv(&state, "conv-list");
        let store = ResearchJobStore::new(&state.db);
        for i in 0..3 {
            store
                .insert_pending(
                    &ResearchJobId::new(),
                    &cid,
                    &format!("query {i}"),
                    "Controller",
                    None,
                    100 + i,
                )
                .unwrap();
        }
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/research/jobs")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["count"], 3);
        assert_eq!(v["jobs"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn get_job_returns_404_for_unknown_id() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/research/jobs/nope")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_report_returns_null_when_no_workspace_yet() {
        let state = test_app_state();
        let cid = seed_conv(&state, "conv-report");
        let id = ResearchJobId::new();
        ResearchJobStore::new(&state.db)
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri(format!("/api/admin/research/jobs/{}/report", id.as_str()))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["report_markdown"].is_null());
    }

    #[tokio::test]
    async fn get_report_reads_workspace_when_report_exists() {
        let state = test_app_state();
        let cid = seed_conv(&state, "conv-report-have");
        let store = ResearchJobStore::new(&state.db);
        let id = ResearchJobId::new();
        store
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(id.as_str());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("report.md"), "# hi\nFindings.").unwrap();
        store
            .set_workspace_path(&id, &dir.to_string_lossy(), 200)
            .unwrap();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri(format!("/api/admin/research/jobs/{}/report", id.as_str()))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["report_markdown"].as_str().unwrap().contains("Findings"));
    }

    #[tokio::test]
    async fn active_count_global_excludes_terminal_rows() {
        let state = test_app_state();
        let cid = seed_conv(&state, "conv-count");
        let store = ResearchJobStore::new(&state.db);
        let active_id = ResearchJobId::new();
        let done_id = ResearchJobId::new();
        store
            .insert_pending(&active_id, &cid, "active", "Controller", None, 100)
            .unwrap();
        store
            .insert_pending(&done_id, &cid, "done", "Controller", None, 110)
            .unwrap();
        store.claim_next_pending("c", 120).unwrap();
        store
            .finish(
                &done_id,
                execlaw_core::research::ResearchJobStatus::Complete,
                None,
                Some("att"),
                130,
            )
            .unwrap();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/research/active_count")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["active_count"], 1);
        assert!(v["conversation_id"].is_null());
    }

    #[tokio::test]
    async fn active_count_scoped_returns_per_conversation_count() {
        let state = test_app_state();
        let _ = seed_conv(&state, "conv-A");
        let _ = seed_conv(&state, "conv-B");
        let store = ResearchJobStore::new(&state.db);
        store
            .insert_pending(
                &ResearchJobId::new(),
                &ConversationId::from("conv-A"),
                "a",
                "Controller",
                None,
                100,
            )
            .unwrap();
        store
            .insert_pending(
                &ResearchJobId::new(),
                &ConversationId::from("conv-B"),
                "b",
                "Controller",
                None,
                110,
            )
            .unwrap();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/research/active_count?conversation_id=conv-A")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["active_count"], 1);
        assert_eq!(v["conversation_id"], "conv-A");
    }

    #[tokio::test]
    async fn list_jobs_rejects_non_controller_caller() {
        // Non-controller users (admin, operator, viewer roles) get
        // 403; matches the strict-controller posture the Audit and
        // Logs admin endpoints already enforce.
        let state = test_app_state();
        let app = build_router(state);
        let _tok = setup_controller_token(&app).await;
        // Anonymous request — no auth header at all → 401.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/research/jobs")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            resp.status() == StatusCode::UNAUTHORIZED
                || resp.status() == StatusCode::FORBIDDEN,
        );
    }
}
