//! Settings → Backends — inference-backend-per-purpose admin
//! routes (Phase 8.5; reshaped in 8.8).
//!
//! There is no create or delete affordance: the set of purposes is
//! a fixed enum (Standard / Small / VoiceSTT / VoiceTTS) and the
//! operator's job is to configure which model + GPU + endpoint
//! serves each one. Standard additionally exposes a
//! `reasoning_enabled` flag that opts the runner into the model's
//! native reasoning mode for that backend. A purpose without a configured
//! backend is rendered as "not configured" by the SPA — the operator
//! fills it in via PUT.
//!
//! Routes:
//!   * `GET  /api/admin/backends` — every configured backend.
//!   * `PUT  /api/admin/backends/{purpose}` — upsert by purpose
//!     (Controller-only, audit-logged).
//!   * `DELETE /api/admin/backends/{purpose}` — clear the slot
//!     (Controller-only). Useful when the operator is wiping a
//!     misconfigured backend and wants a clean re-fill rather than
//!     PUTting a placeholder.

use crate::auth_extract::AuthedUser;
use crate::backend_presets::{self, PresetsResponse};
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, put};
use execlaw_container_manager::{GpuVendor, detect};
use execlaw_core::audit::AuditStore;
use execlaw_core::backends::{
    BackendError, BackendMode, BackendPurpose, BackendRow, BackendStore, BackendUpsert,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct BackendView {
    pub purpose: String,
    pub inference_backend: String,
    #[schema(value_type = serde_json::Value)]
    pub model_spec: serde_json::Value,
    pub gpu_id: Option<String>,
    pub endpoint: Option<String>,
    pub notes: Option<String>,
    /// Phase-8.8: whether reasoning mode is engaged on this
    /// backend. Only meaningful for Standard; the SPA hides the
    /// checkbox for the other purposes.
    pub reasoning_enabled: bool,
    /// True when this purpose accepts a reasoning_enabled value.
    /// Lets the SPA decide whether to render the checkbox without
    /// duplicating the enum locally.
    pub supports_reasoning_toggle: bool,
    /// Phase 12 — lifecycle ownership. One of "external" |
    /// "managed". Defaults to external for every existing row; the
    /// SPA exposes a Mode toggle.
    pub mode: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<&BackendRow> for BackendView {
    fn from(r: &BackendRow) -> Self {
        Self {
            purpose: r.purpose.as_str().to_owned(),
            inference_backend: r.inference_backend.clone(),
            model_spec: r.model_spec_json.clone(),
            gpu_id: r.gpu_id.clone(),
            endpoint: r.endpoint.clone(),
            notes: r.notes.clone(),
            reasoning_enabled: r.reasoning_enabled,
            supports_reasoning_toggle: r.purpose.supports_reasoning_toggle(),
            mode: r.mode.as_str().to_owned(),
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackendListResponse {
    /// One entry per `BackendPurpose::all()` value. Purposes the
    /// operator hasn't configured yet are present with `configured: false`
    /// and the rest of the fields elided so the SPA can render the
    /// fixed-row layout without per-purpose fallbacks.
    pub backends: Vec<BackendListEntry>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackendListEntry {
    pub purpose: String,
    pub configured: bool,
    pub backend: Option<BackendView>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpsertBackendRequest {
    pub inference_backend: String,
    /// Free-form JSON whose schema is plugin-defined. Validated only
    /// for parseability.
    #[schema(value_type = serde_json::Value)]
    pub model_spec: serde_json::Value,
    #[serde(default)]
    pub gpu_id: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Phase-8.8: optional reasoning toggle. Server zeroes it for
    /// purposes that don't support reasoning, so the SPA can pass
    /// it freely without per-purpose branching.
    #[serde(default)]
    pub reasoning_enabled: bool,
    /// Phase 12 — lifecycle ownership. Defaults to "external" so
    /// pre-Phase-12 SPAs that don't send the field keep their
    /// current behaviour. Unknown values are rejected with 400.
    #[serde(default = "default_mode")]
    pub mode: String,
}

fn default_mode() -> String {
    "external".to_owned()
}

impl From<BackendError> for ApiError {
    fn from(err: BackendError) -> Self {
        match err {
            BackendError::Db(e) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "db_error",
                message: e.to_string(),
            },
            BackendError::Sqlite(e) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "db_error",
                message: e.to_string(),
            },
            BackendError::Invalid(msg) => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_backend",
                message: msg,
            },
            BackendError::NotFound(p) => ApiError {
                status: StatusCode::NOT_FOUND,
                code: "backend_not_configured",
                message: format!("no backend configured for purpose '{p}'"),
            },
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/backends",
    responses((status = 200, description = "Inference backends per purpose", body = BackendListResponse)),
    security(("bearer_jwt" = [])),
    tag = "backends"
)]
pub async fn list_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<BackendListResponse>, ApiError> {
    let store = BackendStore::new(&state.db);
    let configured = store.list_all().map_err(ApiError::from)?;
    // Build a fixed-length response — one entry per BackendPurpose
    // — so the SPA renders the same shape regardless of how many
    // are configured.
    let entries: Vec<BackendListEntry> = BackendPurpose::all()
        .iter()
        .map(|p| {
            let row = configured.iter().find(|r| r.purpose == *p);
            BackendListEntry {
                purpose: p.as_str().to_owned(),
                configured: row.is_some(),
                backend: row.map(BackendView::from),
            }
        })
        .collect();
    Ok(Json(BackendListResponse { backends: entries }))
}

#[utoipa::path(
    put,
    path = "/api/admin/backends/{purpose}",
    request_body = UpsertBackendRequest,
    params(("purpose" = String, Path, description = "Backend purpose: Standard | Small | VoiceSTT | VoiceTTS")),
    responses(
        (status = 200, description = "Upserted", body = BackendView),
        (status = 400, description = "Unknown purpose / malformed model_spec"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "backends"
)]
pub async fn upsert_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(purpose): AxumPath<String>,
    Json(req): Json<UpsertBackendRequest>,
) -> Result<Json<BackendView>, ApiError> {
    require_controller(&state, &user)?;
    let purpose = BackendPurpose::parse(&purpose).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_purpose",
        message: format!("'{purpose}' is not a recognised backend purpose"),
    })?;
    if req.inference_backend.trim().is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "missing_backend",
            message: "inference_backend is required".into(),
        });
    }
    let mode = BackendMode::parse(&req.mode).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_mode",
        message: format!(
            "'{}' is not a recognised backend mode (expected 'external' or 'managed')",
            req.mode
        ),
    })?;
    let store = BackendStore::new(&state.db);
    let prior = store.get(purpose).map_err(ApiError::from)?;
    let now = chrono::Utc::now().timestamp();

    // Phase 12.E closure — preserve the supervisor-written endpoint
    // when an operator re-saves a managed row. The SPA always sends
    // `endpoint: null` for managed rows (it's a server-managed
    // field), but blindly persisting null would briefly null out
    // the runtime URL — turning the next turn into a stub fallback
    // until the supervisor reconciles a moment later. If the prior
    // row was managed and had an endpoint, carry it forward; the
    // supervisor will overwrite when the spec change actually
    // forces a respawn.
    let effective_endpoint = match (mode, &req.endpoint, prior.as_ref()) {
        (BackendMode::Managed, None, Some(p)) if p.mode == BackendMode::Managed => {
            p.endpoint.clone()
        }
        _ => req.endpoint.clone(),
    };

    let row = store
        .upsert(
            &BackendUpsert {
                purpose,
                inference_backend: req.inference_backend.clone(),
                model_spec_json: req.model_spec.clone(),
                gpu_id: req.gpu_id.clone(),
                endpoint: effective_endpoint,
                notes: req.notes.clone(),
                reasoning_enabled: req.reasoning_enabled,
                mode,
            },
            now,
        )
        .map_err(ApiError::from)?;

    // Phase 12 closure — kick the supervisor so the reconcile
    // happens within milliseconds instead of waiting up to one
    // tick (~5s). Also reset the per-purpose restart counter so a
    // row that was parked CrashLooping gets a fresh runway after
    // the operator edits its spec. Both are no-ops for external
    // rows; the supervisor only acts on Managed.
    if let Some(sup) = state.backend_supervisor.as_ref() {
        sup.reset_attempts(purpose).await;
        sup.kick();
    }

    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "config_backends",
        purpose.as_str(),
        prior
            .as_ref()
            .map(|r| {
                serde_json::json!({
                    "inference_backend": r.inference_backend,
                    "endpoint": r.endpoint,
                    "gpu_id": r.gpu_id,
                })
            })
            .as_ref(),
        Some(&serde_json::json!({
            "inference_backend": row.inference_backend,
            "endpoint": row.endpoint,
            "gpu_id": row.gpu_id,
        })),
    );

    Ok(Json((&row).into()))
}

#[utoipa::path(
    delete,
    path = "/api/admin/backends/{purpose}",
    params(("purpose" = String, Path, description = "Backend purpose to clear")),
    responses(
        (status = 200, description = "Slot cleared"),
        (status = 400, description = "Unknown purpose"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "Slot was already empty"),
    ),
    security(("bearer_jwt" = [])),
    tag = "backends"
)]
pub async fn clear_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(purpose): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let purpose = BackendPurpose::parse(&purpose).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_purpose",
        message: format!("'{purpose}' is not a recognised backend purpose"),
    })?;
    let store = BackendStore::new(&state.db);
    let prior = store.get(purpose).map_err(ApiError::from)?;
    let removed = store.clear(purpose).map_err(ApiError::from)?;
    if !removed {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "backend_not_configured",
            message: format!("purpose '{}' has no backend to clear", purpose.as_str()),
        });
    }
    if let Some(prior) = prior {
        let _ = AuditStore::new(&state.db).insert(
            &user.user_id,
            "config_backends",
            purpose.as_str(),
            Some(&serde_json::json!({
                "inference_backend": prior.inference_backend,
            })),
            None,
        );
    }
    Ok(StatusCode::OK)
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
            message: "only a Controller can change backend configuration".into(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Phase 12.C — managed-backend status + restart routes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct BackendStatusResponse {
    pub purpose: String,
    pub mode: String,
    /// One of "Pulling" | "Starting" | "Healthy" | "CrashLooping" |
    /// "Stopped" | "NotFound". `Stopped` for external rows.
    pub status: String,
    pub endpoint: Option<String>,
    pub restart_attempts: u32,
    /// True when the supervisor is wired (Docker reachable). False
    /// in dev/test builds; the SPA shows a "Docker unreachable"
    /// notice when this is false and the row is managed.
    pub supervisor_available: bool,
    /// Higher-resolution lifecycle phase. One of "Idle",
    /// "PullingImage", "ContainerStarting", "LoadingModel",
    /// "Healthy", "Failed". Lets the SPA render the right pill
    /// copy + helpful message during long warm-ups (vLLM loading
    /// 27 GB of weights into VRAM is `LoadingModel`, not just
    /// `Starting`).
    pub stage: String,
    /// Wall-clock seconds since the most recent successful spawn.
    /// `None` when no spawn has happened yet. Drives the SPA's
    /// "Loading model · 4m elapsed" copy.
    pub elapsed_secs: Option<u64>,
    /// Most recent meaningful log line from the running container.
    /// Surfaced inline in the status pill / detail so the operator
    /// can see "vLLM is busy loading checkpoint shards (3/12)"
    /// without opening the logs modal.
    pub last_log_line: Option<String>,
    /// In-flight HF model download progress. Populated only while
    /// `stage == "DownloadingModel"`. The SPA renders
    /// `Downloading model · 8.5 / 18.0 GB · 47%` from these
    /// fields.
    pub download_progress: Option<DownloadProgressView>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DownloadProgressView {
    pub bytes_downloaded: u64,
    pub total_bytes: u64,
    pub file_idx: u64,
    pub file_count: u64,
}

#[utoipa::path(
    get,
    path = "/api/admin/backends/{purpose}/status",
    params(("purpose" = String, Path, description = "Backend purpose")),
    responses(
        (status = 200, description = "Live runtime status", body = BackendStatusResponse),
        (status = 400, description = "Unknown purpose"),
    ),
    security(("bearer_jwt" = [])),
    tag = "backends"
)]
pub async fn status_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
    AxumPath(purpose): AxumPath<String>,
) -> Result<Json<BackendStatusResponse>, ApiError> {
    let purpose = BackendPurpose::parse(&purpose).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_purpose",
        message: format!("'{purpose}' is not a recognised backend purpose"),
    })?;

    let store = BackendStore::new(&state.db);
    let row = store.get(purpose).map_err(ApiError::from)?;
    // No row at all → Stopped + external. The supervisor never sees
    // a row that doesn't exist; the SPA still wants a response so
    // the status pill renders deterministically.
    let (mode, endpoint) = match row {
        Some(r) => (r.mode, r.endpoint),
        None => (BackendMode::External, None),
    };

    let supervisor_available = state.backend_supervisor.is_some();
    let (status, restart_attempts, stage, elapsed_secs, last_log_line, download_progress) =
        if let Some(sup) = state.backend_supervisor.as_ref() {
            let snap = sup.snapshot_status().await;
            let entry = snap.iter().find(|s| s.purpose == purpose);
            match entry {
                Some(e) => (
                    runtime_status_str(&e.status).to_owned(),
                    e.restart_attempts,
                    e.stage.as_str().to_owned(),
                    e.elapsed_secs,
                    e.last_log_line.clone(),
                    e.download_progress.map(|p| DownloadProgressView {
                        bytes_downloaded: p.bytes_downloaded,
                        total_bytes: p.total_bytes,
                        file_idx: p.file_idx as u64,
                        file_count: p.file_count as u64,
                    }),
                ),
                None => ("Stopped".to_owned(), 0, "Idle".to_owned(), None, None, None),
            }
        } else {
            ("Stopped".to_owned(), 0, "Idle".to_owned(), None, None, None)
        };

    Ok(Json(BackendStatusResponse {
        purpose: purpose.as_str().to_owned(),
        mode: mode.as_str().to_owned(),
        status,
        endpoint,
        restart_attempts,
        supervisor_available,
        stage,
        elapsed_secs,
        last_log_line,
        download_progress,
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackendLogsResponse {
    pub purpose: String,
    /// Captured log tail. Empty string when there is no log material
    /// available (no managed container ever spawned, or supervisor
    /// not running). The SPA renders that as "no logs yet" rather
    /// than treating it as an error.
    pub logs: String,
    /// True when the supervisor is wired. False ⇒ the response
    /// `logs` is always empty; the SPA renders a "Docker
    /// unreachable" notice in that case.
    pub supervisor_available: bool,
    /// Whether this snapshot came from a live, still-running
    /// container or from the supervisor's CrashLooping cache. Lets
    /// the SPA prefix the panel with "(crashed)" so the operator
    /// knows they're reading post-mortem output, not a tail-f.
    pub from_cache: bool,
}

#[utoipa::path(
    get,
    path = "/api/admin/backends/{purpose}/logs",
    params(
        ("purpose" = String, Path, description = "Backend purpose"),
        ("lines" = Option<u32>, Query, description = "Tail length, default 200, max 2000"),
    ),
    responses(
        (status = 200, description = "Container log tail", body = BackendLogsResponse),
        (status = 400, description = "Unknown purpose"),
    ),
    security(("bearer_jwt" = [])),
    tag = "backends"
)]
pub async fn logs_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
    AxumPath(purpose): AxumPath<String>,
    Query(q): Query<LogsQuery>,
) -> Result<Json<BackendLogsResponse>, ApiError> {
    let purpose = BackendPurpose::parse(&purpose).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_purpose",
        message: format!("'{purpose}' is not a recognised backend purpose"),
    })?;
    let lines = q.lines.unwrap_or(200).min(2000) as usize;
    let supervisor_available = state.backend_supervisor.is_some();
    let logs = match state.backend_supervisor.as_ref() {
        Some(sup) => sup.logs(purpose, lines).await.map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "supervisor_error",
            message: e.to_string(),
        })?,
        None => String::new(),
    };
    // The supervisor's logs() returns the live tail when a handle is
    // present and the cached tail otherwise. We don't have a clean
    // "is the container still alive?" signal here without a second
    // RPC; infer it from the status snapshot.
    let from_cache = match state.backend_supervisor.as_ref() {
        Some(sup) => {
            let snap = sup.snapshot_status().await;
            snap.iter()
                .find(|s| s.purpose == purpose)
                .map(|s| {
                    !matches!(
                        s.status,
                        execlaw_container_manager::ServiceStatus::Healthy
                            | execlaw_container_manager::ServiceStatus::Starting
                            | execlaw_container_manager::ServiceStatus::Pulling
                    )
                })
                .unwrap_or(true)
        }
        None => true,
    };
    Ok(Json(BackendLogsResponse {
        purpose: purpose.as_str().to_owned(),
        logs,
        supervisor_available,
        from_cache,
    }))
}

#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    pub lines: Option<u32>,
}

#[utoipa::path(
    post,
    path = "/api/admin/backends/{purpose}/restart",
    params(("purpose" = String, Path, description = "Backend purpose")),
    responses(
        (status = 200, description = "Restart queued"),
        (status = 400, description = "Unknown purpose"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 503, description = "Supervisor not running (Docker unreachable)"),
    ),
    security(("bearer_jwt" = [])),
    tag = "backends"
)]
pub async fn restart_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(purpose): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let purpose = BackendPurpose::parse(&purpose).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_purpose",
        message: format!("'{purpose}' is not a recognised backend purpose"),
    })?;
    let sup = state.backend_supervisor.as_ref().ok_or_else(|| ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "supervisor_unavailable",
        message: "backend supervisor is not running (Docker unreachable?)".into(),
    })?;
    sup.restart(purpose).await.map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "supervisor_error",
        message: e.to_string(),
    })?;
    Ok(StatusCode::OK)
}

fn runtime_status_str(s: &execlaw_container_manager::ServiceStatus) -> &'static str {
    use execlaw_container_manager::ServiceStatus::*;
    match s {
        Pulling => "Pulling",
        Starting => "Starting",
        Healthy => "Healthy",
        CrashLooping { .. } => "CrashLooping",
        Stopped => "Stopped",
        NotFound => "NotFound",
    }
}

// ---------------------------------------------------------------------------
// Phase 13.B.1 — managed-backend preset wizard endpoint
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PresetsQuery {
    /// Backend purpose: Standard | Small | VoiceSTT | VoiceTTS.
    pub purpose: String,
}

/// Pure helper that turns a `(purpose, detected_vendors)` pair into a
/// PresetsResponse. Split out from `presets_handler` so tests can
/// pin the input regardless of the host's `/sys` tree — the handler
/// stays a thin axum wrapper that only does the sysfs scan.
fn build_presets_response(
    purpose: BackendPurpose,
    detected_vendors: &[GpuVendor],
) -> PresetsResponse {
    let presets = backend_presets::presets_for(purpose, detected_vendors);

    // Stringify + dedupe. A multi-card host (two NVIDIA GPUs) would
    // otherwise yield duplicate "nvidia" entries that React keys by
    // and warns about.
    let mut detected_str: Vec<String> = Vec::new();
    for v in detected_vendors {
        let s = match v {
            GpuVendor::Nvidia => "nvidia",
            GpuVendor::Intel => "intel",
            GpuVendor::Amd => "amd",
            GpuVendor::Apple => "apple",
            GpuVendor::Unknown => continue,
        };
        if !detected_str.iter().any(|existing| existing == s) {
            detected_str.push(s.to_owned());
        }
    }

    PresetsResponse {
        purpose: purpose.as_str().to_owned(),
        detected_vendors: detected_str,
        presets,
    }
}

/// `GET /api/admin/backends/presets?purpose=VoiceSTT` — returns the
/// curated preset list for the given purpose, with per-preset
/// `recommended` flags driven by a fresh sysfs Tier-1 hardware scan.
///
/// The wizard calls this once on Mount. It only reads — no DB writes,
/// no supervisor side-effects — so it doesn't need Controller role
/// (the existing `AuthedUser` extractor is enough; `PUT` on the same
/// row still requires Controller).
#[utoipa::path(
    get,
    path = "/api/admin/backends/presets",
    params(("purpose" = String, Query, description = "Backend purpose: Standard | Small | VoiceSTT | VoiceTTS")),
    responses(
        (status = 200, description = "Hardware-aware preset list", body = PresetsResponse),
        (status = 400, description = "Unknown purpose"),
    ),
    security(("bearer_jwt" = [])),
    tag = "backends"
)]
pub async fn presets_handler(
    _user: AuthedUser,
    Query(q): Query<PresetsQuery>,
) -> Result<Json<PresetsResponse>, ApiError> {
    let purpose = BackendPurpose::parse(&q.purpose).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_purpose",
        message: format!("'{}' is not a recognised backend purpose", q.purpose),
    })?;

    // Phase 14 — cross-platform GPU probe via `hardware-query`. WMI
    // on Windows, sysfs on Linux, IOKit on macOS. Failure modes
    // downgrade to "no GPU detected" so the wizard recommends CPU
    // (correct on a host where the probe couldn't reach the OS).
    let profile = detect();
    let detected_vendors: Vec<GpuVendor> = profile.gpus.iter().map(|g| g.vendor).collect();

    Ok(Json(build_presets_response(purpose, &detected_vendors)))
}

pub fn backends_router() -> Router<AppState> {
    use axum::routing::post;
    Router::new()
        .route("/api/admin/backends", get(list_handler))
        .route("/api/admin/backends/presets", get(presets_handler))
        .route(
            "/api/admin/backends/{purpose}",
            put(upsert_handler).delete(clear_handler),
        )
        .route("/api/admin/backends/{purpose}/status", get(status_handler))
        .route(
            "/api/admin/backends/{purpose}/capabilities",
            get(capabilities_handler),
        )
        .route("/api/admin/backends/{purpose}/logs", get(logs_handler))
        .route(
            "/api/admin/backends/{purpose}/restart",
            post(restart_handler),
        )
}

/// Runtime-detected capabilities for a backend purpose. The SPA reads
/// this on chat-shell mount to decide whether to surface the image-
/// attach affordance in the Composer. The probe hits
/// `GET /v1/models` on the resolved inference endpoint to discover
/// the model id loaded server-side, then applies the curated
/// `is_known_multimodal_model` pattern matcher. Errors fall through
/// as `reachable: false`, which the SPA renders as "no probe yet —
/// no image affordance" rather than a hard failure.
#[derive(Debug, Serialize, ToSchema)]
pub struct BackendCapabilitiesResponse {
    pub purpose: String,
    /// Endpoint we probed. Useful for the SPA's debug surface — an
    /// operator can see whether the probe hit the managed-supervisor
    /// URL vs the operator-typed external URL.
    pub endpoint: Option<String>,
    /// Was the `/v1/models` call reachable AND non-error? When false
    /// the SPA assumes no multimodal support (graceful degradation).
    pub reachable: bool,
    /// First model id reported by the backend's `/v1/models` data
    /// array. None when the probe didn't land or the list was empty.
    pub model_id: Option<String>,
    /// True when `model_id` matches a known vision-capable family.
    pub multimodal: bool,
    /// 2026-05-15 — SPA target for the long-edge dimension after
    /// client-side downscale. Picked from the host's detected VRAM:
    ///   * < 32 GB on any single GPU → 1024 px (24 GB-class card —
    ///     KV cache is tight enough that 1 K vision tokens per image
    ///     is the right trade).
    ///   * 32–64 GB → 1536 px (most operators land here on the
    ///     L40-class card the project ships against).
    ///   * ≥ 64 GB → 2048 px (H100 / multi-A100, no practical
    ///     ceiling on a single image's token cost).
    /// Non-multimodal backends always return 0 — there's nothing to
    /// downscale to and the SPA shouldn't surface the affordance.
    pub recommended_image_edge: u32,
    /// One-line operator-facing diagnostic when the probe didn't
    /// succeed — e.g. "endpoint not configured", "HTTP 503", "decode
    /// error: …". Empty string on success.
    pub error: String,
}

/// Pick the SPA's downscale target from the host's detected VRAM.
/// Conservative: when we can't read GPU memory at all, we assume a
/// 24 GB-class card and return 1024 so an operator on a 4090 / 3090
/// doesn't blow their KV-cache budget on a single 4 K image.
fn recommended_image_edge_for_host() -> u32 {
    let profile = execlaw_container_manager::detect();
    let max_vram_gb: u64 = profile
        .gpus
        .iter()
        .filter_map(|g| g.memory_mb)
        .max()
        .map(|mb| mb / 1024)
        .unwrap_or(0);
    if max_vram_gb >= 64 {
        2048
    } else if max_vram_gb >= 32 {
        1536
    } else {
        1024
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/backends/{purpose}/capabilities",
    params(("purpose" = String, Path, description = "Backend purpose")),
    responses(
        (status = 200, description = "Runtime capability probe", body = BackendCapabilitiesResponse),
        (status = 400, description = "Unknown purpose"),
    ),
    security(("bearer_jwt" = [])),
    tag = "backends"
)]
pub async fn capabilities_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
    AxumPath(purpose): AxumPath<String>,
) -> Result<Json<BackendCapabilitiesResponse>, ApiError> {
    let purpose = BackendPurpose::parse(&purpose).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "unknown_purpose",
        message: format!("'{purpose}' is not a recognised backend purpose"),
    })?;

    // Resolve the live endpoint the same way the chat path does —
    // managed-mode rows pick up the supervisor-written URL, external
    // rows pick up the operator-typed value. A purpose that has no
    // row at all returns `reachable=false` so the SPA's hooks can
    // surface "not configured" without crashing.
    let resolved = state.inference.resolve(&state.db, purpose);
    let endpoint = resolved.as_ref().map(|r| r.endpoint.clone());
    let Some(resolved) = resolved else {
        return Ok(Json(BackendCapabilitiesResponse {
            purpose: purpose.as_str().to_owned(),
            endpoint: None,
            reachable: false,
            model_id: None,
            multimodal: false,
            recommended_image_edge: 0,
            error: "endpoint not configured".to_owned(),
        }));
    };

    let host_edge = recommended_image_edge_for_host();

    match resolved.client.list_models().await {
        Ok(list) => {
            // Prefer the model id the resolver paired with this row
            // (it's the source of truth for what we ship to
            // /chat/completions). Fall back to the first /v1/models
            // entry when the resolver returned an empty model id.
            let resolver_model = resolved.model_id.trim().to_owned();
            let probe_model = list.data.first().map(|m| m.id.clone());
            let model_id = if !resolver_model.is_empty() {
                Some(resolver_model)
            } else {
                probe_model.clone()
            };
            let multimodal = match (model_id.as_deref(), probe_model.as_deref()) {
                (Some(id), _) => execlaw_inference_api::is_known_multimodal_model(id),
                (None, Some(id)) => execlaw_inference_api::is_known_multimodal_model(id),
                _ => false,
            };
            Ok(Json(BackendCapabilitiesResponse {
                purpose: purpose.as_str().to_owned(),
                endpoint,
                reachable: true,
                model_id,
                multimodal,
                recommended_image_edge: if multimodal { host_edge } else { 0 },
                error: String::new(),
            }))
        }
        Err(e) => {
            let multimodal = execlaw_inference_api::is_known_multimodal_model(&resolved.model_id);
            Ok(Json(BackendCapabilitiesResponse {
                purpose: purpose.as_str().to_owned(),
                endpoint,
                reachable: false,
                model_id: Some(resolved.model_id.clone()).filter(|s| !s.is_empty()),
                // Fall back to id-only check when the probe didn't
                // land but the resolver had a model id from the row
                // spec — an operator who saved Qwen3.6 still gets the
                // attach affordance even if the backend is briefly
                // unreachable.
                multimodal,
                recommended_image_edge: if multimodal { host_edge } else { 0 },
                error: format!("probe failed: {e}"),
            }))
        }
    }
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

    fn upsert_body() -> serde_json::Value {
        serde_json::json!({
            "inference_backend": "service-vllm",
            "model_spec": {"model": "Qwen3.5-27B-AWQ"},
            "gpu_id": "0",
            "endpoint": "http://127.0.0.1:8000/v1",
            "notes": null
        })
    }

    #[tokio::test]
    async fn list_returns_one_entry_per_purpose_even_when_unconfigured() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/backends")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let arr = v["backends"].as_array().unwrap();
        // One entry per `BackendPurpose` variant regardless of how
        // many are filled in. Anchored to the enum's `all()` length
        // so adding a variant (Vision landed via M5) updates this
        // expectation automatically.
        assert_eq!(arr.len(), BackendPurpose::all().len());
        for entry in arr {
            assert_eq!(entry["configured"], false);
            assert!(entry["backend"].is_null());
        }
    }

    #[tokio::test]
    async fn upsert_creates_then_updates_in_place() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Standard")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(upsert_body().to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Second PUT updates the same row.
        let mut body2 = upsert_body();
        body2["endpoint"] = serde_json::Value::String("http://127.0.0.1:9000/v1".into());
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Standard")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body2.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["endpoint"], "http://127.0.0.1:9000/v1");

        // List now shows configured = true for Standard.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/backends")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let standard = v["backends"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["purpose"] == "Standard")
            .unwrap();
        assert_eq!(standard["configured"], true);
    }

    #[tokio::test]
    async fn upsert_rejects_unknown_purpose() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Hallucinated")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(upsert_body().to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn clear_returns_404_for_unconfigured_slot() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/api/admin/backends/Small")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn clear_round_trip() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        // Upsert then clear.
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/admin/backends/Standard")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(upsert_body().to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/api/admin/backends/Standard")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn list_returns_default_external_mode_for_existing_rows() {
        // Phase 12.A: a backend created without a `mode` field
        // (legacy SPA, missing field) defaults to external. The
        // BackendView in the response carries that string back so
        // the SPA can render the toggle.
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/admin/backends/Standard")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(upsert_body().to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/admin/backends")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let standard = v["backends"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["purpose"] == "Standard")
            .unwrap();
        assert_eq!(
            standard["backend"]["mode"], "external",
            "absent mode field defaults to external on the wire"
        );
    }

    #[tokio::test]
    async fn upsert_managed_round_trips_mode() {
        // Phase 12.A: an explicit `mode: "managed"` round-trips on
        // the View. The endpoint can be null at this stage — the
        // BackendSupervisor (Phase 12.C) writes it back after spawn.
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let mut body = upsert_body();
        body["mode"] = serde_json::Value::String("managed".into());
        body["endpoint"] = serde_json::Value::Null;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Standard")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["mode"], "managed");
        assert!(v["endpoint"].is_null());
    }

    #[tokio::test]
    async fn re_saving_managed_row_preserves_supervisor_written_endpoint() {
        // Phase 12.E closure — the SPA sends `endpoint: null` when
        // saving a managed row (server-managed field). The handler
        // must NOT overwrite a supervisor-written endpoint with
        // null, because the next turn would briefly stub-fall-back
        // before the supervisor reconciles.
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;

        // Create a managed row.
        let mut body = upsert_body();
        body["mode"] = serde_json::Value::String("managed".into());
        body["endpoint"] = serde_json::Value::Null;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Standard")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        // Simulate supervisor writing the endpoint back.
        execlaw_core::backends::BackendStore::new(
            &execlaw_core::Database::open(&execlaw_core::db::DbConfig::in_memory_unencrypted())
                .unwrap(),
        );
        // Manually patch via the actual app's DB by reissuing as
        // an upsert with explicit endpoint — the cleanest test
        // shape since we don't have a handle to the app's db
        // through the router.
        let mut body2 = upsert_body();
        body2["mode"] = serde_json::Value::String("managed".into());
        body2["endpoint"] = serde_json::Value::String("http://127.0.0.1:8101".into());
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Standard")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body2.to_string()))
            .unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        // Re-save with mode=managed + endpoint=null — what the SPA
        // sends. Handler must preserve the prior endpoint.
        let mut body3 = upsert_body();
        body3["mode"] = serde_json::Value::String("managed".into());
        body3["endpoint"] = serde_json::Value::Null;
        body3["notes"] = serde_json::Value::String("just changed notes".into());
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Standard")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body3.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v["endpoint"], "http://127.0.0.1:8101",
            "managed re-save with null endpoint must preserve the prior endpoint"
        );
        assert_eq!(v["notes"], "just changed notes");
    }

    #[tokio::test]
    async fn flipping_managed_to_external_lets_operator_endpoint_survive() {
        // Mirror of the supervisor test — confirm the upsert
        // handler also doesn't accidentally clobber when the
        // operator flips mode while typing a fresh external URL.
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;

        // managed → endpoint set by sim
        let mut body = upsert_body();
        body["mode"] = serde_json::Value::String("managed".into());
        body["endpoint"] = serde_json::Value::String("http://127.0.0.1:8101".into());
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Standard")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        // Flip to external with a fresh URL — handler must take the
        // operator's input verbatim, NOT preserve the prior managed
        // URL.
        let mut body2 = upsert_body();
        body2["mode"] = serde_json::Value::String("external".into());
        body2["endpoint"] = serde_json::Value::String("http://192.168.1.50:8000/v1".into());
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Standard")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body2.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["mode"], "external");
        assert_eq!(v["endpoint"], "http://192.168.1.50:8000/v1");
    }

    #[tokio::test]
    async fn upsert_rejects_unknown_mode() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let mut body = upsert_body();
        body["mode"] = serde_json::Value::String("kubernetes".into());
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Standard")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn status_route_reports_supervisor_unavailable_in_test_state() {
        // Phase 12.C — test_app_state ships supervisor=None, so the
        // status route must surface that fact (so the SPA can show a
        // "Docker unreachable" notice).
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/backends/Standard/status")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["mode"], "external");
        assert_eq!(v["status"], "Stopped");
        assert_eq!(v["supervisor_available"], false);
    }

    #[tokio::test]
    async fn status_route_with_managed_supervisor_reports_healthy() {
        // Build an AppState with a mock-controller-backed supervisor
        // and a managed Standard row. After two reconcile passes the
        // supervisor reports Healthy and the status route surfaces
        // it.
        use execlaw_container_manager::MockServiceController;
        use execlaw_core::backends::BackendUpsert;
        let mut state = test_app_state();
        let mock = std::sync::Arc::new(MockServiceController::new());
        let sup = crate::backend_supervisor::BackendSupervisor::new(state.db.clone(), mock.clone());
        state.backend_supervisor = Some(sup.clone());

        // Seed a managed Standard row.
        execlaw_core::backends::BackendStore::new(&state.db)
            .upsert(
                &BackendUpsert {
                    purpose: BackendPurpose::Standard,
                    inference_backend: "service-vllm".into(),
                    model_spec_json: serde_json::json!({
                        "image": "vllm:test",
                        "args": ["--model", "X"],
                        "container_port": 8000,
                    }),
                    gpu_id: Some("0".into()),
                    endpoint: None,
                    notes: None,
                    reasoning_enabled: true,
                    mode: BackendMode::Managed,
                },
                100,
            )
            .unwrap();
        sup.reconcile_once().await;
        sup.reconcile_once().await;

        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/backends/Standard/status")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["mode"], "managed");
        assert_eq!(v["status"], "Healthy");
        assert_eq!(v["supervisor_available"], true);
        assert!(
            v["endpoint"]
                .as_str()
                .unwrap()
                .starts_with("http://127.0.0.1:")
        );
    }

    #[tokio::test]
    async fn logs_route_serves_supervisor_cached_tail() {
        // Phase 14 follow-up — the /logs endpoint reads the
        // supervisor's per-purpose cached log tail (captured the
        // last time the container CrashLooped). With no supervisor
        // wired the route still 200s with empty logs +
        // supervisor_available=false so the SPA renders a
        // "Docker offline" notice rather than a generic error toast.
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/backends/Standard/logs?lines=50")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["purpose"], "Standard");
        assert_eq!(v["supervisor_available"], false);
        assert_eq!(v["logs"], "");
    }

    #[tokio::test]
    async fn logs_route_rejects_unknown_purpose() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/backends/Hallucinated/logs")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn restart_route_503_when_supervisor_absent() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/backends/Standard/restart")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn reasoning_flag_is_zeroed_for_non_standard_purposes() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        // Send reasoning_enabled = true on a Small backend; server
        // must zero it because Small doesn't expose the toggle.
        let mut body = upsert_body();
        body["reasoning_enabled"] = serde_json::Value::Bool(true);
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Small")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["reasoning_enabled"], false);
        assert_eq!(v["supports_reasoning_toggle"], false);

        // Standard with reasoning_enabled = true round-trips truthy.
        let mut body2 = upsert_body();
        body2["reasoning_enabled"] = serde_json::Value::Bool(true);
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/backends/Standard")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body2.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["reasoning_enabled"], true);
        assert_eq!(v["supports_reasoning_toggle"], true);
    }

    // ---------- Phase 13.B.1 — presets endpoint --------------------------

    #[test]
    fn build_presets_response_recommends_cpu_on_no_gpu_host() {
        // Pure helper — no sysfs dependency, so this asserts the
        // CPU recommendation regardless of the dev host's hardware.
        let resp = build_presets_response(BackendPurpose::VoiceStt, &[]);
        assert_eq!(resp.purpose, "VoiceSTT");
        assert!(resp.detected_vendors.is_empty());
        let cpu = resp
            .presets
            .iter()
            .find(|p| p.preset.id == "whisper-cpu")
            .expect("cpu preset present");
        assert!(cpu.recommended);
        // GPU presets are still returned but unrecommended.
        for p in resp.presets.iter() {
            if p.preset.vendor != "cpu" {
                assert!(!p.recommended);
            }
        }
    }

    #[test]
    fn build_presets_response_recommends_intel_on_arc_host() {
        let resp = build_presets_response(BackendPurpose::VoiceStt, &[GpuVendor::Intel]);
        assert_eq!(resp.detected_vendors, vec!["intel"]);
        let intel = resp
            .presets
            .iter()
            .find(|p| p.preset.id == "whisper-openvino-arc")
            .unwrap();
        assert!(intel.recommended);
        let cpu = resp
            .presets
            .iter()
            .find(|p| p.preset.id == "whisper-cpu")
            .unwrap();
        assert!(!cpu.recommended);
    }

    #[test]
    fn build_presets_response_dedupes_multi_card_vendors() {
        // Two NVIDIA cards must not produce ["nvidia", "nvidia"]
        // — the SPA keys badges by vendor string and would warn
        // about duplicate React keys + render two identical chips.
        let resp = build_presets_response(
            BackendPurpose::VoiceStt,
            &[GpuVendor::Nvidia, GpuVendor::Nvidia, GpuVendor::Intel],
        );
        assert_eq!(resp.detected_vendors, vec!["nvidia", "intel"]);
    }

    #[test]
    fn build_presets_response_skips_unknown_vendor() {
        // Unknown vendor (e.g. unsupported PCI id) — no recommendation
        // signal, but it shouldn't appear in the detected_vendors
        // string list either since the SPA's vendor-label table
        // can't render it.
        let resp = build_presets_response(BackendPurpose::VoiceStt, &[GpuVendor::Unknown]);
        assert!(resp.detected_vendors.is_empty());
    }

    #[test]
    fn build_presets_response_carries_inference_backend_per_preset() {
        // Audit fix: SPA used to guess inference_backend from the
        // preset id prefix. Server is now the single source of
        // truth. Spot-check a few.
        let resp = build_presets_response(BackendPurpose::Standard, &[GpuVendor::Nvidia]);
        let vllm = resp
            .presets
            .iter()
            .find(|p| p.preset.id == "vllm-cuda")
            .unwrap();
        assert_eq!(vllm.preset.inference_backend, "service-vllm");
    }

    #[tokio::test]
    async fn presets_route_returns_per_purpose_library() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/backends/presets?purpose=VoiceSTT")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["purpose"], "VoiceSTT");

        let presets = v["presets"].as_array().unwrap();
        // VoiceSTT ships nvidia + intel + cpu presets per the locked decisions.
        assert!(presets.iter().any(|p| p["id"] == "whisper-faster-cuda"));
        assert!(presets.iter().any(|p| p["id"] == "whisper-openvino-arc"));
        assert!(presets.iter().any(|p| p["id"] == "whisper-cpu"));

        // Whisper preset must surface model_size with default = small.
        let whisper = presets
            .iter()
            .find(|p| p["id"] == "whisper-faster-cuda")
            .unwrap();
        let fields = whisper["fields"].as_array().unwrap();
        let model_size = fields
            .iter()
            .find(|f| f["kind"] == "model_size")
            .expect("Whisper preset must expose model_size");
        assert_eq!(model_size["default"], "small");
    }

    #[tokio::test]
    async fn presets_route_recommends_cpu_when_no_gpu_detected() {
        // Test hosts without /sys (e.g. macOS CI) get an empty profile,
        // which the wizard interprets as "no GPU detected" and the CPU
        // preset is the recommended one.
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/backends/presets?purpose=VoiceTTS")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        // On a no-GPU host the SPA shows the CPU card highlighted.
        // We can't assert "exactly the CPU preset is recommended"
        // because the dev rig may have an NVIDIA GPU; instead assert
        // every recommended preset's vendor matches at least one
        // detected vendor or 'cpu' iff no GPU was detected.
        let detected = v["detected_vendors"].as_array().unwrap();
        let presets = v["presets"].as_array().unwrap();
        for p in presets {
            if p["recommended"].as_bool().unwrap_or(false) {
                let vendor = p["vendor"].as_str().unwrap();
                if detected.is_empty() {
                    assert_eq!(vendor, "cpu", "no-GPU host must only recommend CPU presets");
                } else {
                    assert!(
                        detected.iter().any(|d| d.as_str() == Some(vendor))
                            || vendor == "cpu" && detected.is_empty(),
                        "recommended preset's vendor must match a detected GPU"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn presets_route_rejects_unknown_purpose() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/backends/presets?purpose=Hallucinated")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn presets_route_requires_auth() {
        let app = build_router(test_app_state());
        // No setup, no token — the auth extractor should reject.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/backends/presets?purpose=Standard")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
            "expected auth rejection, got {}",
            resp.status()
        );
    }
}
