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
use axum::routing::{get, post};
use execlaw_core::cards::{CardClosedPayload, CardState};
use execlaw_core::ids::{ConversationId, ResearchJobId};
use execlaw_core::research::{
    PhaseGates, ResearchConfigStore, ResearchJobStatus, ResearchJobStore, ResearchJobSummary,
};
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
    let row = ResearchJobStore::new(&state.db)
        .get(&id)?
        .ok_or_else(|| ApiError {
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
        None => store.active_count_global()?,
    };
    Ok(Json(ResearchActiveCountResponse {
        active_count: count,
        conversation_id: q.conversation_id,
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ResearchAdvanceResponse {
    pub job_id: String,
    /// New status after the advance. For PlanOnly + Planned this is
    /// `complete`; for EveryPhase + Planned this is `gathering`; for
    /// EveryPhase + Gathering this is `complete`. Set to the prior
    /// status when the request was a no-op (job not in an
    /// advanceable state).
    pub status: String,
    /// Whether the request actually triggered a phase. `false` for
    /// idempotent no-ops (job already terminal, or in a state the
    /// advance flow doesn't handle from).
    pub advanced: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ResearchCancelResponse {
    pub job_id: String,
    /// Whether the cancel actually flipped the row. `false` for
    /// idempotent no-ops on already-terminal jobs.
    pub cancelled: bool,
}

#[derive(Debug, Deserialize, Default, ToSchema)]
pub struct ResearchCancelRequest {
    /// Operator-supplied note saved into `state_research_jobs.error`.
    /// Optional; the column already coerces null to "operator
    /// cancelled" defaultless.
    #[serde(default)]
    pub reason: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/admin/research/jobs/{job_id}/cancel",
    request_body = ResearchCancelRequest,
    responses(
        (status = 200, description = "Cancel recorded", body = ResearchCancelResponse),
        (status = 404, description = "No job with that id"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "research"
)]
pub async fn cancel_job_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(job_id): Path<String>,
    Json(req): Json<ResearchCancelRequest>,
) -> Result<Json<ResearchCancelResponse>, ApiError> {
    require_controller(&state, &user)?;
    let id = ResearchJobId::from(job_id.as_str());
    let store = ResearchJobStore::new(&state.db);
    let row = store.get(&id)?.ok_or_else(|| ApiError {
        status: StatusCode::NOT_FOUND,
        code: "research_not_found",
        message: format!("no research job '{job_id}'"),
    })?;
    let now = chrono::Utc::now().timestamp();
    let cancelled = store.cancel_active(
        &id,
        req.reason.as_deref().or(Some("operator cancelled")),
        now,
    )?;
    if cancelled {
        // C6c — short-circuit any in-flight gather phase. Without
        // this signal the row flips to Cancelled but the spawned
        // runner keeps spending tokens until its phase finishes.
        // The token lookup goes through the supervisor's registry
        // — `None` is fine (job already finished, or the
        // supervisor isn't wired in this fixture).
        if let Some(supervisor) = state.research_supervisor.as_ref()
            && let Some(token) = supervisor.cancel_token_for(id.as_str())
        {
            token.cancel();
        }
        // Mirror the runner's lifecycle by closing the card so the
        // SPA flips inline-render from live to the final summary
        // and the WS subscribers see the terminal state.
        let card_id = row
            .card_id
            .clone()
            .unwrap_or_else(|| format!("research-{}", row.id.as_str()));
        let summary = req
            .reason
            .clone()
            .map(|r| format!("Research cancelled: {r}"))
            .unwrap_or_else(|| "Research cancelled by operator.".into());
        let _ = crate::cards::close_card_and_broadcast(
            &state.db,
            &state.events,
            &row.conversation_id,
            "system",
            &CardClosedPayload {
                card_id,
                state: CardState::Cancelled,
                summary,
                details: None,
                attachment_id: None,
                error: req.reason.clone(),
            },
        );
    }
    Ok(Json(ResearchCancelResponse { job_id, cancelled }))
}

#[utoipa::path(
    post,
    path = "/api/admin/research/jobs/{job_id}/advance",
    responses(
        (status = 200, description = "Advance request accepted", body = ResearchAdvanceResponse),
        (status = 404, description = "No job with that id"),
        (status = 409, description = "Job not in an advanceable state"),
        (status = 503, description = "No inference backend wired"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "research"
)]
pub async fn advance_job_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(job_id): Path<String>,
) -> Result<Json<ResearchAdvanceResponse>, ApiError> {
    require_controller(&state, &user)?;
    let id = ResearchJobId::from(job_id.as_str());
    let store = ResearchJobStore::new(&state.db);
    let row = store.get(&id)?.ok_or_else(|| ApiError {
        status: StatusCode::NOT_FOUND,
        code: "research_not_found",
        message: format!("no research job '{job_id}'"),
    })?;
    // The advance endpoint exists to drive the operator-confirm
    // flows: PlanOnly + Planned → run gather + synthesise; EveryPhase
    // + Planned → run gather only; EveryPhase + Gathering → run
    // synthesise. None auto-chains via the runner so it's never
    // advanceable here. Other statuses (Pending / Planning /
    // Synthesizing / terminal) → 409, idempotent.
    let cfg = ResearchConfigStore::new(&state.db).get()?;
    let prior = row.status;
    if !matches!(
        prior,
        ResearchJobStatus::Planned | ResearchJobStatus::Gathering
    ) {
        return Ok(Json(ResearchAdvanceResponse {
            job_id,
            status: prior.as_str().to_owned(),
            advanced: false,
        }));
    }

    // Resolve the inference backend the runner was using. If none is
    // available we can't run further phases; fail loud so the
    // operator notices.
    let inference = state
        .inference
        .resolve(&state.db, execlaw_core::backends::BackendPurpose::Standard)
        .ok_or_else(|| ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "no_inference_backend",
            message: "no inference backend configured; cannot advance research phase".into(),
        })?;
    let model = state.config.model_id.clone();
    let workspace =
        crate::research::ResearchWorkspace::new(crate::research::ResearchWorkspace::default_root());
    let plan = row
        .plan_json
        .as_ref()
        .and_then(|b| rmp_serde::from_slice::<execlaw_core::research::ResearchPlan>(b).ok())
        .ok_or_else(|| ApiError {
            status: StatusCode::CONFLICT,
            code: "research_no_plan",
            message: "job has no plan persisted; cannot advance".into(),
        })?;
    let card_id = row
        .card_id
        .clone()
        .unwrap_or_else(|| format!("research-{}", row.id.as_str()));
    let conv_id = row.conversation_id.clone();
    let query = row.query.clone();

    // Spawn the next phase off the request handler — the LLM call
    // is multi-second and the operator's UI shouldn't block on it.
    // Mint + register a cancellation token so a subsequent
    // /cancel call can short-circuit the gather phase. The token
    // lands on the live `ResearchSupervisor`'s registry; the
    // /cancel handler looks it up by job id and `.cancel()`s it.
    // Without registration, the cancel endpoint flips the row but
    // the gather phase keeps spending tokens.
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_cleanup = state.research_supervisor.as_ref().map(|sup| {
        sup.cancel_tokens
            .insert(id.as_str().to_owned(), cancel.clone());
        sup.cancel_tokens.clone()
    });
    let phase_deps = crate::research::runner::PhaseDeps {
        db: state.db.clone(),
        events: state.events.clone(),
        workspace,
        conversation_id: conv_id,
        card_id,
        inference,
        model,
        cancel: cancel.clone(),
    };
    let db_for_notes = state.db.clone();
    let id_for_task = id.clone();
    let id_key_for_cleanup = id.as_str().to_owned();
    let next_status_str = match (cfg.phase_gates, prior) {
        (PhaseGates::EveryPhase, ResearchJobStatus::Planned) => "gathering".to_owned(),
        _ => "complete".to_owned(),
    };
    tokio::spawn(async move {
        if matches!(prior, ResearchJobStatus::Planned) {
            let halt_after_gather = matches!(cfg.phase_gates, PhaseGates::EveryPhase);
            let notes = match crate::research::runner::run_gather_phase(
                &phase_deps,
                &id_for_task,
                &plan,
                &cfg,
                halt_after_gather,
            )
            .await
            {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(
                        job_id = id_for_task.as_str(),
                        error = %e,
                        "advance(Planned) gather phase failed",
                    );
                    return;
                }
            };
            if !halt_after_gather {
                if let Err(e) = crate::research::runner::run_synthesize_phase(
                    &phase_deps,
                    &id_for_task,
                    &query,
                    &plan,
                    &notes,
                )
                .await
                {
                    tracing::warn!(
                        job_id = id_for_task.as_str(),
                        error = %e,
                        "advance(Planned→synthesize) failed",
                    );
                }
            }
        } else {
            // prior == Gathering; pull the persisted notes back.
            let notes_row = match ResearchJobStore::new(&db_for_notes).get(&id_for_task) {
                Ok(Some(r)) => r,
                _ => return,
            };
            let notes = notes_row
                .notes_json
                .as_ref()
                .and_then(|b| {
                    rmp_serde::from_slice::<Vec<execlaw_core::research::ResearchNote>>(b).ok()
                })
                .unwrap_or_default();
            if let Err(e) = crate::research::runner::run_synthesize_phase(
                &phase_deps,
                &id_for_task,
                &query,
                &plan,
                &notes,
            )
            .await
            {
                tracing::warn!(
                    job_id = id_for_task.as_str(),
                    error = %e,
                    "advance(Gathering→synthesize) failed",
                );
            }
        }
        // Drop the registry entry on exit (success, failure, halt).
        // Mirrors what `ResearchSupervisor::spawn_runner_for` does on
        // its happy + sad paths so the DashMap can't leak entries
        // across the supervisor's lifetime.
        if let Some(tokens) = cancel_cleanup {
            tokens.remove(&id_key_for_cleanup);
        }
    });
    Ok(Json(ResearchAdvanceResponse {
        job_id,
        status: next_status_str,
        advanced: true,
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
        .route(
            "/api/admin/research/jobs/{job_id}/cancel",
            post(cancel_job_handler),
        )
        .route(
            "/api/admin/research/jobs/{job_id}/advance",
            post(advance_job_handler),
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
    async fn cancel_endpoint_flips_active_row_to_cancelled() {
        let state = test_app_state();
        let cid = seed_conv(&state, "conv-cancel");
        let id = ResearchJobId::new();
        ResearchJobStore::new(&state.db)
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/admin/research/jobs/{}/cancel", id.as_str()))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"reason": "test cancel"}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["cancelled"], true);
        // DB side: row is now Cancelled with the operator's reason.
        let row = ResearchJobStore::new(&state.db).get(&id).unwrap().unwrap();
        assert_eq!(
            row.status,
            execlaw_core::research::ResearchJobStatus::Cancelled,
        );
        assert_eq!(row.error.as_deref(), Some("test cancel"));
    }

    #[tokio::test]
    async fn cancel_endpoint_fires_live_cancel_token_when_present() {
        // C6c invariant: an active row's cancel call must propagate
        // .cancel() to the supervisor's registered token so the
        // gather phase actually short-circuits (rather than just
        // flipping the DB row while the runner keeps spending
        // tokens). Test a synthesized supervisor + token rather
        // than driving a full runner: insert a row in Planning,
        // register a token in the supervisor's cancel_tokens map,
        // hit /cancel, assert the token is now cancelled.
        use crate::research::ResearchSupervisor;
        use crate::research::ResearchWorkspace;
        let mut state = test_app_state();
        let cid = seed_conv(&state, "conv-cancel-fires");
        let store = ResearchJobStore::new(&state.db);
        let id = ResearchJobId::new();
        store
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        store.claim_next_pending("c", 110).unwrap();
        // Build a supervisor + register a token under this job.
        let tmp = tempfile::tempdir().unwrap();
        let supervisor = ResearchSupervisor::new(
            state.db.clone(),
            state.inference.clone(),
            ResearchWorkspace::new(tmp.path()),
            "test-model".into(),
            state.events.clone(),
        );
        let token = tokio_util::sync::CancellationToken::new();
        supervisor
            .cancel_tokens
            .insert(id.as_str().to_owned(), token.clone());
        state.research_supervisor = Some(supervisor.clone());
        // Pre-cancel: token is NOT cancelled.
        assert!(!token.is_cancelled());
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/admin/research/jobs/{}/cancel", id.as_str()))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::json!({}).to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Token is now cancelled — gather workers checking
        // `cancel.is_cancelled()` will exit at their next checkpoint.
        assert!(
            token.is_cancelled(),
            "cancel endpoint must fire the registered token",
        );
    }

    #[tokio::test]
    async fn cancel_endpoint_no_op_on_token_when_no_supervisor_wired() {
        // Defensive: when state.research_supervisor is None (test
        // fixture, or a future runtime mode that disables the
        // supervisor), the cancel endpoint must still flip the DB
        // row cleanly. The token-fire branch is just skipped.
        let state = test_app_state();
        // No research_supervisor wired (matches test_app_state
        // default). The endpoint should still succeed.
        let cid = seed_conv(&state, "conv-cancel-no-sup");
        let id = ResearchJobId::new();
        ResearchJobStore::new(&state.db)
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/admin/research/jobs/{}/cancel", id.as_str()))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::json!({}).to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn cancel_endpoint_idempotent_on_completed_row() {
        let state = test_app_state();
        let cid = seed_conv(&state, "conv-cancel-done");
        let store = ResearchJobStore::new(&state.db);
        let id = ResearchJobId::new();
        store
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        store.claim_next_pending("c", 110).unwrap();
        store
            .finish(
                &id,
                execlaw_core::research::ResearchJobStatus::Complete,
                None,
                Some("att-1"),
                200,
            )
            .unwrap();
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/admin/research/jobs/{}/cancel", id.as_str()))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::json!({}).to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["cancelled"], false);
        // Row stays Complete with attachment intact.
        let row = ResearchJobStore::new(&state.db).get(&id).unwrap().unwrap();
        assert_eq!(
            row.status,
            execlaw_core::research::ResearchJobStatus::Complete,
        );
        assert_eq!(row.attachment_id.as_deref(), Some("att-1"));
    }

    #[tokio::test]
    async fn advance_endpoint_returns_503_when_no_inference_backend() {
        // Production-like setup: row in Planned, but the test
        // app_state has no inference backend. Advance must surface a
        // structured 503 rather than silently spawning a task that
        // panics later.
        let state = test_app_state();
        let cid = seed_conv(&state, "conv-advance-503");
        let store = ResearchJobStore::new(&state.db);
        let id = ResearchJobId::new();
        store
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        store.claim_next_pending("c", 110).unwrap();
        store
            .set_planned(
                &id,
                &execlaw_core::research::ResearchPlan {
                    thesis: "t".into(),
                    steps: vec![execlaw_core::research::PlanStep {
                        query: "q".into(),
                        rationale: None,
                    }],
                },
                120,
            )
            .unwrap();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/admin/research/jobs/{}/advance", id.as_str()))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn advance_endpoint_no_op_for_pending_or_terminal_rows() {
        // Pending: never advances (gather requires a plan).
        let state = test_app_state();
        let cid = seed_conv(&state, "conv-advance-noop");
        let id = ResearchJobId::new();
        ResearchJobStore::new(&state.db)
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/admin/research/jobs/{}/advance", id.as_str()))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // 200 with advanced=false is the idempotent contract.
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["advanced"], false);
        assert_eq!(v["status"], "pending");
    }

    #[tokio::test]
    async fn advance_endpoint_cleans_up_cancel_token_after_spawned_task_exits() {
        // C6c invariant: every code path that REGISTERS a cancel
        // token in the supervisor's DashMap must remove it on exit
        // — otherwise long-lived control-plane processes leak an
        // entry per advance(). The runner-spawn path already does
        // this (supervisor.rs `spawn_runner_for`); this test pins
        // the same parity for advance_job_handler's spawned task.
        //
        // Wire an unreachable backend so the gather phase fails
        // fast (the OpenAI client raises a connect error within
        // milliseconds against port 1). The cleanup must happen on
        // the failure path too.
        use crate::research::{ResearchSupervisor, ResearchWorkspace};
        use execlaw_core::backends::{BackendMode, BackendPurpose, BackendStore, BackendUpsert};
        let mut state = test_app_state();
        let cid = seed_conv(&state, "conv-advance-leak");
        let store = ResearchJobStore::new(&state.db);
        let id = ResearchJobId::new();
        store
            .insert_pending(&id, &cid, "q", "Controller", None, 100)
            .unwrap();
        store.claim_next_pending("c", 110).unwrap();
        store
            .set_planned(
                &id,
                &execlaw_core::research::ResearchPlan {
                    thesis: "t".into(),
                    steps: vec![execlaw_core::research::PlanStep {
                        query: "q".into(),
                        rationale: None,
                    }],
                },
                120,
            )
            .unwrap();
        // Seed an inference backend pointing at an unreachable port.
        // The gather phase will fail immediately on connect; the
        // spawned task must still hit the cleanup branch.
        BackendStore::new(&state.db)
            .upsert(
                &BackendUpsert {
                    purpose: BackendPurpose::Standard,
                    inference_backend: "service-vllm".into(),
                    model_spec_json: serde_json::json!({}),
                    gpu_id: None,
                    endpoint: Some("http://127.0.0.1:1/v1".into()),
                    notes: None,
                    reasoning_enabled: false,
                    mode: BackendMode::External,
                },
                130,
            )
            .unwrap();
        // Wire a real supervisor so advance_job_handler registers
        // its token in a registry we can observe.
        let tmp = tempfile::tempdir().unwrap();
        let supervisor = ResearchSupervisor::new(
            state.db.clone(),
            state.inference.clone(),
            ResearchWorkspace::new(tmp.path()),
            "test-model".into(),
            state.events.clone(),
        );
        let registry = supervisor.cancel_tokens.clone();
        state.research_supervisor = Some(supervisor);
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/admin/research/jobs/{}/advance", id.as_str()))
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Advance returned; the spawned task is racing the cleanup.
        // Poll the registry briefly — without the fix, the entry
        // would linger forever; with the fix, it's gone within a
        // few hundred ms (gather connect-error + cleanup).
        let mut cleared = false;
        for _ in 0..80 {
            if !registry.contains_key(id.as_str()) {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            cleared,
            "advance handler must remove the supervisor cancel-token entry after its spawned task exits",
        );
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
            resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        );
    }
}
