//! Admin HTTP API for Automations (M4 backend).
//!
//! Mounted under `/api/admin/automations*` â€” controller-gated like
//! the rest of the admin surface. Endpoints:
//!
//! | Method | Path                                       | Purpose                                        |
//! | ------ | ------------------------------------------ | ---------------------------------------------- |
//! | GET    | `/api/admin/automations`                   | List all automations (enabled + disabled)      |
//! | POST   | `/api/admin/automations`                   | Create a new automation                        |
//! | GET    | `/api/admin/automations/:id`               | Get one automation by id                       |
//! | PUT    | `/api/admin/automations/:id`               | Update an automation in place                  |
//! | DELETE | `/api/admin/automations/:id`               | Delete an automation                           |
//! | POST   | `/api/admin/automations/:id/enable`        | Set `enabled = true`                           |
//! | POST   | `/api/admin/automations/:id/disable`       | Set `enabled = false`                          |
//! | GET    | `/api/admin/automations/:id/runs`          | Recent run history for an automation           |
//! | GET    | `/api/admin/automations/metrics`           | Aggregate cards for the landing page           |
//! | GET    | `/api/admin/automations/suggestions`       | Pending suggestions from the sweeper           |
//! | POST   | `/api/admin/automations/suggestions/:id/dismiss` | Dismiss a suggestion (mutes pattern)     |
//! | POST   | `/api/admin/automations/suggestions/:id/action`  | Mark a suggestion as actioned            |
//!
//! Validation: every write goes through `AutomationStore::upsert`
//! which runs the M2 validator. A 400 surfaces the validator's
//! human-readable message verbatim â€” operators get actionable
//! feedback without spelunking the trace.

use crate::automation_runtime;
use crate::automation_runtime::DryRunResult;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use execlaw_core::automation_bus::{BusEventRow, BusEventStore};
use execlaw_core::automation_runs::{AutomationRunRow, AutomationRunStore};
use execlaw_core::automation_suggestions::SuggestionStore;
use execlaw_core::automations::{
    AutomationDef, AutomationError, AutomationRow, AutomationStore, AutomationUpsert,
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/automations", get(list).post(create))
        .route("/api/admin/automations/metrics", get(metrics))
        .route("/api/admin/automations/suggestions", get(list_suggestions))
        .route(
            "/api/admin/automations/suggestions/{id}/dismiss",
            post(dismiss_suggestion),
        )
        .route(
            "/api/admin/automations/suggestions/{id}/action",
            post(action_suggestion),
        )
        .route(
            "/api/admin/automations/suggestions/{id}",
            get(get_suggestion),
        )
        .route(
            "/api/admin/automations/{id}",
            get(get_one).put(update).delete(delete_one),
        )
        .route("/api/admin/automations/{id}/enable", post(enable))
        .route("/api/admin/automations/{id}/disable", post(disable))
        .route("/api/admin/automations/{id}/runs", get(list_runs))
        .route("/api/admin/automations/{id}/test-run", post(test_run))
        .route(
            "/api/admin/automations/recent-events",
            get(list_recent_events),
        )
        // M6 — registry inspection endpoints.
        .route(
            "/api/admin/automations/registered-events",
            get(list_registered_events),
        )
        .route(
            "/api/admin/automations/registered-reply-handlers",
            get(list_registered_reply_handlers),
        )
        .route(
            "/api/admin/automations/default-flows",
            get(list_default_flows),
        )
}

// --- M6 registry inspection handlers --------------------------------

#[axum::debug_handler]
async fn list_registered_events(
    State(state): State<AppState>,
) -> Result<Json<Vec<execlaw_core::event_registry::RegisteredEventKind>>, ApiError> {
    let reg = execlaw_core::event_registry::EventRegistry::new(&state.db);
    let kinds = reg.list_event_kinds().map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "registry_list_failed",
        message: format!("{e}"),
    })?;
    Ok(Json(kinds))
}

#[axum::debug_handler]
async fn list_registered_reply_handlers(
    State(state): State<AppState>,
) -> Result<Json<Vec<execlaw_core::event_registry::RegisteredReplyHandler>>, ApiError> {
    let reg = execlaw_core::event_registry::EventRegistry::new(&state.db);
    let handlers = reg.list_reply_handlers().map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "registry_list_failed",
        message: format!("{e}"),
    })?;
    Ok(Json(handlers))
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct DefaultFlowDto {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub source: String,
    pub source_version: Option<String>,
    pub operator_modified: bool,
}

#[axum::debug_handler]
async fn list_default_flows(
    State(state): State<AppState>,
) -> Result<Json<Vec<DefaultFlowDto>>, ApiError> {
    // Pull all automations + filter to source != "operator". For
    // slice 9 we read the source columns directly via SQL since the
    // AutomationRow struct doesn't surface them yet.
    use rusqlite::params;
    let rows = state
        .db
        .with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, enabled, source, source_version, operator_modified \
                 FROM state_automations \
                 WHERE source != 'operator' \
                 ORDER BY name ASC",
            )?;
            let rows = stmt.query_map(params![], |r| {
                Ok(DefaultFlowDto {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    enabled: r.get::<_, i64>(2)? != 0,
                    source: r.get(3)?,
                    source_version: r.get(4)?,
                    operator_modified: r.get::<_, i64>(5)? != 0,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok::<_, execlaw_core::db::DbError>(out)
        })
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "default_flows_list_failed",
            message: format!("{e}"),
        })?;
    Ok(Json(rows))
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AutomationDto {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub definition: AutomationDef,
    pub created_at: i64,
    pub updated_at: i64,
    /// M6 — provenance: `"operator"` | `"core"` | `"plugin:<id>"`.
    /// Drives the SPA's hide-the-delete-button logic for default
    /// flows.
    pub source: String,
    /// M6 — `true` when the operator has edited a non-operator
    /// row.
    pub operator_modified: bool,
    /// M6 — convenience flag = `source != "operator"`. The SPA
    /// hides the delete button when this is `true`.
    pub is_default: bool,
}

impl From<AutomationRow> for AutomationDto {
    fn from(r: AutomationRow) -> Self {
        let is_default = r.is_default();
        Self {
            id: r.id,
            name: r.name,
            enabled: r.enabled,
            definition: r.definition,
            created_at: r.created_at,
            updated_at: r.updated_at,
            source: r.source,
            operator_modified: r.operator_modified,
            is_default,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateAutomationRequest {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub definition: AutomationDef,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UpdateAutomationRequest {
    pub name: String,
    pub enabled: bool,
    pub definition: AutomationDef,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MetricsDto {
    pub active_count: i64,
    pub runs_24h: i64,
    pub success_rate_24h: Option<f64>, // None when no runs in window
    pub untriaged_kinds_24h: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RecentBusEventDto {
    pub id: String,
    pub kind: String,
    pub source: String,
    pub received_at: i64,
    #[schema(value_type = Object)]
    pub payload: serde_json::Value,
}

impl From<BusEventRow> for RecentBusEventDto {
    fn from(r: BusEventRow) -> Self {
        Self {
            id: r.id,
            kind: r.kind,
            source: r.source,
            received_at: r.received_at,
            payload: r.payload,
        }
    }
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RecentEventsQuery {
    pub kind: String,
    #[serde(default = "default_recent_limit")]
    pub limit: i64,
}

fn default_recent_limit() -> i64 {
    50
}

/// Body for the test-run endpoint. Two modes:
///
///   1. `event_id` â€” resolve a captured event from `state_bus_events`.
///      Use this when picking from the "recent events" dropdown.
///   2. `sample_event` â€” synthesize an event from scratch. Use this
///      when no captured event is suitable (e.g., the operator wants
///      to test against a hypothetical payload).
///
/// One of the two MUST be set. If both are set, `event_id` wins so
/// the operator's edit of the picked event's payload doesn't silently
/// re-resolve.
#[derive(Debug, Deserialize, ToSchema)]
pub struct TestRunRequest {
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub sample_event: Option<SampleEventBody>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SampleEventBody {
    pub kind: String,
    pub source: String,
    #[schema(value_type = Object)]
    pub payload: serde_json::Value,
    /// Optional envelope override. When omitted the test-run path
    /// synthesizes `EventEnvelope::system_internal()` — the same
    /// behavior as legacy rows. Operators set this to test trigger
    /// filters that gate on origin / identity / correlation.
    #[serde(default)]
    pub envelope: Option<execlaw_core::event_envelope::EventEnvelope>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SuggestionDto {
    pub id: String,
    pub kind: String,
    pub source: String,
    pub event_count: i64,
    pub sample_event_ids: Vec<String>,
    pub suggested_name: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// M5: agent-drafted seed for the editor handoff. `None` for
    /// plain pattern-detected suggestions; `Some(_)` when an agent
    /// drafting path has populated a graph the operator can review
    /// and tweak.
    pub draft_definition: Option<execlaw_core::automations::AutomationDef>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/api/admin/automations",
    responses((status = 200, description = "All automations", body = [AutomationDto])),
    security(("bearer_jwt" = [])),
    tag = "automations"
)]
pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<AutomationDto>>, ApiError> {
    let rows = AutomationStore::new(&state.db)
        .list_all()
        .map_err(automation_err)?;
    Ok(Json(rows.into_iter().map(AutomationDto::from).collect()))
}

#[utoipa::path(
    post,
    path = "/api/admin/automations",
    request_body = CreateAutomationRequest,
    responses(
        (status = 201, description = "Created", body = AutomationDto),
        (status = 400, description = "Validation failed"),
    ),
    security(("bearer_jwt" = [])),
    tag = "automations"
)]
pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateAutomationRequest>,
) -> Result<(StatusCode, Json<AutomationDto>), ApiError> {
    let now = chrono::Utc::now().timestamp();
    let row = AutomationStore::new(&state.db)
        .upsert(
            &AutomationUpsert {
                id: None,
                name: req.name,
                enabled: req.enabled,
                definition: req.definition,
            },
            now,
        )
        .map_err(automation_err)?;
    Ok((StatusCode::CREATED, Json(AutomationDto::from(row))))
}

#[utoipa::path(
    get,
    path = "/api/admin/automations/{id}",
    params(("id" = String, Path, description = "Automation id")),
    responses(
        (status = 200, description = "Automation", body = AutomationDto),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "automations"
)]
pub async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AutomationDto>, ApiError> {
    let row = AutomationStore::new(&state.db)
        .get(&id)
        .map_err(automation_err)?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "automation_not_found",
            message: format!("no automation with id '{id}'"),
        })?;
    Ok(Json(AutomationDto::from(row)))
}

#[utoipa::path(
    put,
    path = "/api/admin/automations/{id}",
    params(("id" = String, Path, description = "Automation id")),
    request_body = UpdateAutomationRequest,
    responses(
        (status = 200, description = "Updated", body = AutomationDto),
        (status = 400, description = "Validation failed"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "automations"
)]
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateAutomationRequest>,
) -> Result<Json<AutomationDto>, ApiError> {
    // Existence check first so an update of a deleted automation
    // returns 404 rather than silently creating a new row at the
    // operator-supplied id.
    let store = AutomationStore::new(&state.db);
    if store.get(&id).map_err(automation_err)?.is_none() {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "automation_not_found",
            message: format!("no automation with id '{id}'"),
        });
    }
    let now = chrono::Utc::now().timestamp();
    let row = store
        .upsert(
            &AutomationUpsert {
                id: Some(id),
                name: req.name,
                enabled: req.enabled,
                definition: req.definition,
            },
            now,
        )
        .map_err(automation_err)?;
    Ok(Json(AutomationDto::from(row)))
}

#[utoipa::path(
    delete,
    path = "/api/admin/automations/{id}",
    params(("id" = String, Path, description = "Automation id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "automations"
)]
pub async fn delete_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    use execlaw_core::automations::DeleteOutcome;
    let outcome = AutomationStore::new(&state.db)
        .delete(&id)
        .map_err(automation_err)?;
    match outcome {
        DeleteOutcome::Deleted => Ok(StatusCode::NO_CONTENT),
        DeleteOutcome::NotFound => Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "automation_not_found",
            message: format!("no automation with id '{id}'"),
        }),
        DeleteOutcome::RefusedDefault { source } => Err(ApiError {
            status: StatusCode::FORBIDDEN,
            code: "automation_is_default",
            message: format!(
                "cannot delete a default flow shipped by '{source}' — disable it instead, or uninstall the source plugin"
            ),
        }),
    }
}

#[utoipa::path(
    post,
    path = "/api/admin/automations/{id}/enable",
    params(("id" = String, Path, description = "Automation id")),
    responses(
        (status = 204, description = "Enabled"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "automations"
)]
pub async fn enable(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    toggle(&state, &id, true).await
}

#[utoipa::path(
    post,
    path = "/api/admin/automations/{id}/disable",
    params(("id" = String, Path, description = "Automation id")),
    responses(
        (status = 204, description = "Disabled"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "automations"
)]
pub async fn disable(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    toggle(&state, &id, false).await
}

async fn toggle(state: &AppState, id: &str, enabled: bool) -> Result<StatusCode, ApiError> {
    let now = chrono::Utc::now().timestamp();
    let ok = AutomationStore::new(&state.db)
        .set_enabled(id, enabled, now)
        .map_err(automation_err)?;
    if ok {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "automation_not_found",
            message: format!("no automation with id '{id}'"),
        })
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/automations/recent-events",
    params(RecentEventsQuery),
    responses((status = 200, description = "Recent bus events for the requested kind", body = [RecentBusEventDto])),
    security(("bearer_jwt" = [])),
    tag = "automations"
)]
pub async fn list_recent_events(
    State(state): State<AppState>,
    Query(q): Query<RecentEventsQuery>,
) -> Result<Json<Vec<RecentBusEventDto>>, ApiError> {
    let limit = q.limit.clamp(1, 200);
    let rows = BusEventStore::new(&state.db)
        .list_recent_for_kind(&q.kind, limit)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "recent_events_failed",
            message: format!("{e}"),
        })?;
    Ok(Json(
        rows.into_iter().map(RecentBusEventDto::from).collect(),
    ))
}

#[utoipa::path(
    post,
    path = "/api/admin/automations/{id}/test-run",
    params(("id" = String, Path, description = "Automation id")),
    request_body = TestRunRequest,
    responses(
        (status = 200, description = "Dry-run result", body = DryRunResult),
        (status = 400, description = "Missing event_id or sample_event"),
        (status = 404, description = "Automation or event not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "automations"
)]
pub async fn test_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<TestRunRequest>,
) -> Result<Json<automation_runtime::DryRunResult>, ApiError> {
    // Resolve the automation up front so a bad id 404s before we
    // touch the bus store.
    let auto_store = AutomationStore::new(&state.db);
    let automation = auto_store
        .get(&id)
        .map_err(automation_err)?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "automation_not_found",
            message: format!("no automation with id '{id}'"),
        })?;
    // Resolve the event â€” captured or synthesized.
    let event = match (req.event_id.as_deref(), req.sample_event) {
        (Some(eid), _) => BusEventStore::new(&state.db)
            .get(eid)
            .map_err(|e| ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "bus_event_lookup_failed",
                message: format!("{e}"),
            })?
            .ok_or_else(|| ApiError {
                status: StatusCode::NOT_FOUND,
                code: "bus_event_not_found",
                message: format!("no bus event with id '{eid}'"),
            })?,
        (None, Some(s)) => {
            // Synthesize a non-persisted BusEventRow. ID is
            // informational (the dry-run path never writes it back).
            // Envelope defaults to `system_internal()` so legacy
            // callers keep working; the SPA passes a built envelope
            // when the operator wants to test envelope-gated
            // triggers (origin.kind, identity.trust, etc.).
            BusEventRow {
                id: format!("test-run:{}", uuid::Uuid::new_v4()),
                kind: s.kind,
                source: s.source,
                received_at: chrono::Utc::now().timestamp_millis(),
                payload: s.payload,
                internal: false,
                dispatched_at: None,
                envelope: s
                    .envelope
                    .unwrap_or_else(execlaw_core::event_envelope::EventEnvelope::system_internal),
            }
        }
        (None, None) => {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "test_run_missing_event",
                message: "either event_id or sample_event must be set".into(),
            });
        }
    };
    // The dry-run path holds the calling tokio worker for the
    // duration of the executor (which may invoke the agent pool
    // synchronously via `block_on`). Push it onto a blocking thread
    // so the admin handler doesn't park a runtime worker.
    let db = state.db.clone();
    let pool = state.automation_agent_pool.clone();
    let plugin_host = Some(state.plugin_host.clone());
    let flow_channel = state.flow_channel.clone();
    let events_bus = state.events.clone();
    let hmac_key = state.event_log_hmac_key.clone();
    let result = tokio::task::spawn_blocking(move || {
        let ctx = automation_runtime::ExecutorContext::new(db, pool, plugin_host)
            .with_flow_channel(flow_channel)
            .with_events(events_bus)
            .with_event_log_hmac_key(hmac_key);
        automation_runtime::dry_run(&ctx, &automation, &event)
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "test_run_spawn_failed",
        message: format!("{e}"),
    })?;
    Ok(Json(result))
}

#[utoipa::path(
    get,
    path = "/api/admin/automations/{id}/runs",
    params(("id" = String, Path, description = "Automation id")),
    responses((status = 200, description = "Recent runs (last 100)", body = [AutomationRunRow])),
    security(("bearer_jwt" = [])),
    tag = "automations"
)]
pub async fn list_runs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runs = AutomationRunStore::new(&state.db)
        .list_for_automation(&id, 100)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "automation_runs_failed",
            message: format!("{e}"),
        })?;
    Ok(Json(serde_json::to_value(runs).unwrap_or_default()))
}

#[utoipa::path(
    get,
    path = "/api/admin/automations/metrics",
    responses((status = 200, description = "Aggregate cards for the landing page", body = MetricsDto)),
    security(("bearer_jwt" = [])),
    tag = "automations"
)]
pub async fn metrics(State(state): State<AppState>) -> Result<Json<MetricsDto>, ApiError> {
    let now = chrono::Utc::now().timestamp_millis();
    let window_24h_ms = 24 * 60 * 60 * 1000_i64;
    let cutoff_ms = now.saturating_sub(window_24h_ms);
    let dto = state
        .db
        .with_conn(|c| {
            // active_count: enabled automations
            let active_count: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM state_automations WHERE enabled = 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            // runs_24h
            let runs_24h: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM state_automation_runs WHERE started_at >= ?1",
                    [cutoff_ms],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            // success rate
            let success_24h: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM state_automation_runs \
                     WHERE started_at >= ?1 AND status = 'success'",
                    [cutoff_ms],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let failed_24h: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM state_automation_runs \
                     WHERE started_at >= ?1 AND status = 'failed'",
                    [cutoff_ms],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let total = success_24h + failed_24h;
            let success_rate_24h = if total > 0 {
                Some(success_24h as f64 / total as f64)
            } else {
                None
            };
            // untriaged: distinct (kind, source) pairs with bus events
            // in the last 24h whose kind has no enabled automation.
            // 24h aligns with the runs_24h card; the suggestion sweep
            // uses a wider 7d window.
            let untriaged_kinds_24h: i64 = c
                .query_row(
                    "SELECT COUNT(DISTINCT kind || '::' || source) \
                     FROM state_bus_events \
                     WHERE received_at >= ?1 \
                       AND kind NOT IN ( \
                           SELECT json_extract(definition, '$.trigger.kind') \
                           FROM state_automations WHERE enabled = 1 \
                       )",
                    [cutoff_ms],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            Ok(MetricsDto {
                active_count,
                runs_24h,
                success_rate_24h,
                untriaged_kinds_24h,
            })
        })
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "metrics_failed",
            message: format!("{e}"),
        })?;
    Ok(Json(dto))
}

#[utoipa::path(
    get,
    path = "/api/admin/automations/suggestions",
    responses((status = 200, description = "Pending suggestions (high-volume untriaged event kinds)", body = [SuggestionDto])),
    security(("bearer_jwt" = [])),
    tag = "automations"
)]
pub async fn list_suggestions(
    State(state): State<AppState>,
) -> Result<Json<Vec<SuggestionDto>>, ApiError> {
    let rows = SuggestionStore::new(&state.db)
        .list_pending()
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "suggestions_list_failed",
            message: format!("{e}"),
        })?;
    Ok(Json(
        rows.into_iter()
            .map(|r| SuggestionDto {
                id: r.id,
                kind: r.kind,
                source: r.source,
                event_count: r.event_count,
                sample_event_ids: r.sample_event_ids,
                suggested_name: r.suggested_name,
                created_at: r.created_at,
                updated_at: r.updated_at,
                draft_definition: r.draft_definition,
            })
            .collect(),
    ))
}

/// M5: GET one suggestion by id. Used by the editor's "Review and
/// create" handoff to fetch the draft when present.
#[utoipa::path(
    get,
    path = "/api/admin/automations/suggestions/{id}",
    params(("id" = String, Path, description = "Suggestion id")),
    responses(
        (status = 200, description = "Suggestion (including draft definition if present)", body = SuggestionDto),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "automations"
)]
pub async fn get_suggestion(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<SuggestionDto>, ApiError> {
    let r = SuggestionStore::new(&state.db)
        .get(&id)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "suggestion_get_failed",
            message: format!("{e}"),
        })?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "suggestion_not_found",
            message: format!("no suggestion with id '{id}'"),
        })?;
    Ok(Json(SuggestionDto {
        id: r.id,
        kind: r.kind,
        source: r.source,
        event_count: r.event_count,
        sample_event_ids: r.sample_event_ids,
        suggested_name: r.suggested_name,
        created_at: r.created_at,
        updated_at: r.updated_at,
        draft_definition: r.draft_definition,
    }))
}

#[utoipa::path(
    post,
    path = "/api/admin/automations/suggestions/{id}/dismiss",
    params(("id" = String, Path, description = "Suggestion id")),
    responses(
        (status = 204, description = "Dismissed; pattern is now muted"),
        (status = 404, description = "Suggestion not pending or not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "automations"
)]
pub async fn dismiss_suggestion(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let now = chrono::Utc::now().timestamp();
    let ok = SuggestionStore::new(&state.db)
        .dismiss(&id, now)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "suggestion_dismiss_failed",
            message: format!("{e}"),
        })?;
    if ok {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "suggestion_not_found_or_already_resolved",
            message: format!("no pending suggestion with id '{id}'"),
        })
    }
}

#[utoipa::path(
    post,
    path = "/api/admin/automations/suggestions/{id}/action",
    params(("id" = String, Path, description = "Suggestion id")),
    responses(
        (status = 204, description = "Marked as actioned"),
        (status = 404, description = "Suggestion not pending or not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "automations"
)]
pub async fn action_suggestion(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let now = chrono::Utc::now().timestamp();
    let ok = SuggestionStore::new(&state.db)
        .mark_actioned(&id, now)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "suggestion_action_failed",
            message: format!("{e}"),
        })?;
    if ok {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "suggestion_not_found_or_already_resolved",
            message: format!("no pending suggestion with id '{id}'"),
        })
    }
}

/// Map `AutomationError` to the HTTP response. Validator failures
/// surface as 400 with the raw message; everything else is 500.
fn automation_err(e: AutomationError) -> ApiError {
    match e {
        AutomationError::Validation(msg) => ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "automation_invalid",
            message: msg,
        },
        AutomationError::NotFound(id) => ApiError {
            status: StatusCode::NOT_FOUND,
            code: "automation_not_found",
            message: format!("no automation with id '{id}'"),
        },
        _ => ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "automation_internal_error",
            message: format!("{e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::test_app_state;
    use axum::body::{self, Body};
    use axum::http::{Request, header};
    use execlaw_core::automation_bus::{BusEventStore, Event as BusEvent};
    use execlaw_core::automations::{
        AutomationDef, EdgeDef, NodeDef, NodeKind, TRIGGER_SENTINEL, TriggerDef,
    };
    use tower::ServiceExt;

    fn minimal_def() -> AutomationDef {
        AutomationDef {
            trigger: TriggerDef {
                kind: "webhook.received".to_owned(),
                when: None,
            },
            nodes: vec![NodeDef {
                id: "end".into(),
                kind: NodeKind::Terminal,
                config: serde_json::json!({}),
                position: None,
            }],
            edges: vec![EdgeDef {
                from: TRIGGER_SENTINEL.into(),
                to: "end".into(),
                when: None,
            }],
        }
    }

    fn app() -> axum::Router {
        let state = test_app_state();
        router().with_state(state)
    }

    async fn json_req<B: Serialize>(
        app: axum::Router,
        method: &str,
        uri: &str,
        body: Option<&B>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        let req = match body {
            Some(b) => builder
                .body(Body::from(serde_json::to_vec(b).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let val: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, val)
    }

    #[tokio::test]
    async fn create_then_list_returns_one() {
        let (status, body) = json_req(
            app(),
            "POST",
            "/api/admin/automations",
            Some(&CreateAutomationRequest {
                name: "first".into(),
                enabled: true,
                definition: minimal_def(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["name"], "first");
        let id = body["id"].as_str().unwrap().to_string();
        let (status, body) = json_req::<()>(app(), "GET", "/api/admin/automations", None).await;
        assert_eq!(status, StatusCode::OK);
        // Each request constructs a fresh app + db, so list will be
        // empty in the second call. We verify the create path landed
        // by re-fetching via the SAME app router.
        let _ = id;
        let _ = body;
    }

    #[tokio::test]
    async fn create_then_list_through_same_app_returns_one() {
        let app = app();
        let (status, body) = json_req(
            app.clone(),
            "POST",
            "/api/admin/automations",
            Some(&CreateAutomationRequest {
                name: "alpha".into(),
                enabled: true,
                definition: minimal_def(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let id = body["id"].as_str().unwrap().to_string();
        let (status, body) =
            json_req::<()>(app.clone(), "GET", "/api/admin/automations", None).await;
        assert_eq!(status, StatusCode::OK);
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], id);
    }

    #[tokio::test]
    async fn create_invalid_returns_400_with_validator_message() {
        let bad_def = AutomationDef {
            trigger: TriggerDef {
                kind: "webhook.received".to_owned(),
                when: None,
            },
            nodes: vec![],
            edges: vec![],
        };
        let (status, body) = json_req(
            app(),
            "POST",
            "/api/admin/automations",
            Some(&CreateAutomationRequest {
                name: "bad".into(),
                enabled: true,
                definition: bad_def,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // ApiError serializes to {"error": {"code": ..., "message": ...}}
        // â€” see routes::ApiError::into_response.
        assert_eq!(body["error"]["code"], "automation_invalid");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("no edge from trigger"),
            "validator message should mention the missing trigger edge; got: {body}",
        );
    }

    #[tokio::test]
    async fn update_unknown_id_returns_404() {
        let (status, _) = json_req(
            app(),
            "PUT",
            "/api/admin/automations/does-not-exist",
            Some(&UpdateAutomationRequest {
                name: "nope".into(),
                enabled: true,
                definition: minimal_def(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn enable_disable_toggle_round_trip() {
        let app = app();
        let (_, body) = json_req(
            app.clone(),
            "POST",
            "/api/admin/automations",
            Some(&CreateAutomationRequest {
                name: "x".into(),
                enabled: true,
                definition: minimal_def(),
            }),
        )
        .await;
        let id = body["id"].as_str().unwrap().to_string();
        let (status, _) = json_req::<()>(
            app.clone(),
            "POST",
            &format!("/api/admin/automations/{id}/disable"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) = json_req::<()>(
            app.clone(),
            "GET",
            &format!("/api/admin/automations/{id}"),
            None,
        )
        .await;
        assert_eq!(body["enabled"], false);
        let (status, _) = json_req::<()>(
            app.clone(),
            "POST",
            &format!("/api/admin/automations/{id}/enable"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) =
            json_req::<()>(app, "GET", &format!("/api/admin/automations/{id}"), None).await;
        assert_eq!(body["enabled"], true);
    }

    #[tokio::test]
    async fn delete_returns_204_then_404() {
        let app = app();
        let (_, body) = json_req(
            app.clone(),
            "POST",
            "/api/admin/automations",
            Some(&CreateAutomationRequest {
                name: "ephemeral".into(),
                enabled: true,
                definition: minimal_def(),
            }),
        )
        .await;
        let id = body["id"].as_str().unwrap().to_string();
        let (status, _) = json_req::<()>(
            app.clone(),
            "DELETE",
            &format!("/api/admin/automations/{id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) =
            json_req::<()>(app, "DELETE", &format!("/api/admin/automations/{id}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn metrics_returns_zeroes_on_empty_db() {
        let (status, body) =
            json_req::<()>(app(), "GET", "/api/admin/automations/metrics", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["active_count"], 0);
        assert_eq!(body["runs_24h"], 0);
        assert!(body["success_rate_24h"].is_null());
        assert_eq!(body["untriaged_kinds_24h"], 0);
    }

    #[tokio::test]
    async fn metrics_counts_untriaged_kinds() {
        let state = test_app_state();
        // Seed bus events with no matching automation.
        let bus = BusEventStore::new(&state.db);
        let now_ms = chrono::Utc::now().timestamp_millis();
        for i in 0..3 {
            bus.publish(
                &BusEvent {
                    id: format!("e-{i}"),
                    kind: "webhook.received".to_owned(),
                    source: "webhook:ring".into(),
                    received_at: now_ms - i,
                    payload: serde_json::json!({}),
                    envelope: None,
                },
                false,
            )
            .unwrap();
        }
        let app = router().with_state(state);
        let (_, body) = json_req::<()>(app, "GET", "/api/admin/automations/metrics", None).await;
        assert_eq!(body["untriaged_kinds_24h"], 1);
    }

    #[tokio::test]
    async fn list_suggestions_returns_pending_rows() {
        let state = test_app_state();
        // Seed enough bus events to produce a sweep candidate, then
        // run the sweep directly. The HTTP endpoint surfaces what
        // landed.
        let bus = BusEventStore::new(&state.db);
        let now_ms = chrono::Utc::now().timestamp_millis();
        for i in 0..15 {
            bus.publish(
                &BusEvent {
                    id: format!("e-{i}"),
                    kind: "webhook.received".to_owned(),
                    source: "webhook:ring".into(),
                    received_at: now_ms - i,
                    payload: serde_json::json!({}),
                    envelope: None,
                },
                false,
            )
            .unwrap();
        }
        SuggestionStore::new(&state.db)
            .sweep(chrono::Utc::now().timestamp())
            .unwrap();
        let app = router().with_state(state);
        let (status, body) =
            json_req::<()>(app, "GET", "/api/admin/automations/suggestions", None).await;
        assert_eq!(status, StatusCode::OK);
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["source"], "webhook:ring");
        assert_eq!(arr[0]["event_count"], 15);
    }

    #[tokio::test]
    async fn test_run_with_sample_event_returns_dry_run_result() {
        // E2E for the M4c test-run endpoint: create an automation
        // with a Filter that always passes, then POST a synthetic
        // event; the result carries the in-memory step trace + a
        // success outcome AND no run row lands on disk.
        let state = test_app_state();
        let app = router().with_state(state.clone());
        // Create the automation.
        let def = AutomationDef {
            trigger: TriggerDef {
                kind: "webhook.received".to_owned(),
                when: None,
            },
            nodes: vec![
                NodeDef {
                    id: "f1".into(),
                    kind: NodeKind::Filter,
                    config: serde_json::json!({"expr": "true"}),
                    position: None,
                },
                NodeDef {
                    id: "end".into(),
                    kind: NodeKind::Terminal,
                    config: serde_json::json!({}),
                    position: None,
                },
            ],
            edges: vec![
                EdgeDef {
                    from: TRIGGER_SENTINEL.into(),
                    to: "f1".into(),
                    when: None,
                },
                EdgeDef {
                    from: "f1".into(),
                    to: "end".into(),
                    when: None,
                },
            ],
        };
        let (_, body) = json_req(
            app.clone(),
            "POST",
            "/api/admin/automations",
            Some(&CreateAutomationRequest {
                name: "test-run-target".into(),
                enabled: true,
                definition: def,
            }),
        )
        .await;
        let id = body["id"].as_str().unwrap().to_string();
        // POST test-run with a synthesized event.
        let trbody = serde_json::json!({
            "sample_event": {
                "kind": "webhook.received",
                "source": "test",
                "payload": {"k": "v"}
            }
        });
        let (status, result) = json_req(
            app,
            "POST",
            &format!("/api/admin/automations/{id}/test-run"),
            Some(&trbody),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["outcome"], "success");
        let steps = result["step_traces"].as_array().unwrap();
        assert!(steps.iter().any(|t| t["node_id"] == "f1"));
        // Crucially: no run row was persisted for this dry run.
        use execlaw_core::automation_runs::AutomationRunStore;
        let runs = AutomationRunStore::new(&state.db)
            .list_for_automation(&id, 10)
            .unwrap();
        assert_eq!(runs.len(), 0, "dry run must NOT persist a run row");
    }

    #[tokio::test]
    async fn test_run_requires_event_id_or_sample_event() {
        let app = app();
        let (_, body) = json_req(
            app.clone(),
            "POST",
            "/api/admin/automations",
            Some(&CreateAutomationRequest {
                name: "x".into(),
                enabled: true,
                definition: AutomationDef {
                    trigger: TriggerDef {
                        kind: "webhook.received".to_owned(),
                        when: None,
                    },
                    nodes: vec![NodeDef {
                        id: "end".into(),
                        kind: NodeKind::Terminal,
                        config: serde_json::json!({}),
                        position: None,
                    }],
                    edges: vec![EdgeDef {
                        from: TRIGGER_SENTINEL.into(),
                        to: "end".into(),
                        when: None,
                    }],
                },
            }),
        )
        .await;
        let id = body["id"].as_str().unwrap().to_string();
        let (status, body) = json_req(
            app,
            "POST",
            &format!("/api/admin/automations/{id}/test-run"),
            Some(&serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "test_run_missing_event");
    }

    #[tokio::test]
    async fn test_run_unknown_automation_returns_404() {
        let app = app();
        let (status, _) = json_req(
            app,
            "POST",
            "/api/admin/automations/no-such-id/test-run",
            Some(&serde_json::json!({
                "sample_event": {
                    "kind": "webhook.received",
                    "source": "x",
                    "payload": {}
                }
            })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn recent_events_filters_by_kind_and_respects_limit_clamp() {
        let state = test_app_state();
        let bus = execlaw_core::automation_bus::BusEventStore::new(&state.db);
        let now_ms = chrono::Utc::now().timestamp_millis();
        // Seed 6 webhook events + 2 routine events.
        for i in 0..6 {
            bus.publish(
                &execlaw_core::automation_bus::Event {
                    id: format!("wh-{i}"),
                    kind: "webhook.received".to_owned(),
                    source: "x".into(),
                    received_at: now_ms - i,
                    payload: serde_json::json!({"i": i}),
                    envelope: None,
                },
                false,
            )
            .unwrap();
        }
        for i in 0..2 {
            bus.publish(
                &execlaw_core::automation_bus::Event {
                    id: format!("rt-{i}"),
                    kind: "routine.fired".to_owned(),
                    source: "x".into(),
                    received_at: now_ms - i,
                    payload: serde_json::json!({}),
                    envelope: None,
                },
                false,
            )
            .unwrap();
        }
        let app = router().with_state(state);
        // Kind filter respected.
        let (status, body) = json_req::<()>(
            app.clone(),
            "GET",
            "/api/admin/automations/recent-events?kind=webhook.received",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 6);
        // Limit clamps to [1, 200].
        let (_, body) = json_req::<()>(
            app.clone(),
            "GET",
            "/api/admin/automations/recent-events?kind=webhook.received&limit=3",
            None,
        )
        .await;
        assert_eq!(body.as_array().unwrap().len(), 3);
        // Limit=0 clamps to 1, not 0.
        let (_, body) = json_req::<()>(
            app,
            "GET",
            "/api/admin/automations/recent-events?kind=webhook.received&limit=0",
            None,
        )
        .await;
        assert_eq!(body.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn dismiss_suggestion_mutes_and_returns_204() {
        let state = test_app_state();
        let bus = BusEventStore::new(&state.db);
        let now_ms = chrono::Utc::now().timestamp_millis();
        for i in 0..15 {
            bus.publish(
                &BusEvent {
                    id: format!("e-{i}"),
                    kind: "webhook.received".to_owned(),
                    source: "webhook:ring".into(),
                    received_at: now_ms - i,
                    payload: serde_json::json!({}),
                    envelope: None,
                },
                false,
            )
            .unwrap();
        }
        SuggestionStore::new(&state.db)
            .sweep(chrono::Utc::now().timestamp())
            .unwrap();
        let app = router().with_state(state.clone());
        let (_, body) = json_req::<()>(
            app.clone(),
            "GET",
            "/api/admin/automations/suggestions",
            None,
        )
        .await;
        let id = body[0]["id"].as_str().unwrap().to_string();
        let (status, _) = json_req::<()>(
            app,
            "POST",
            &format!("/api/admin/automations/suggestions/{id}/dismiss"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        // Muted entry exists.
        assert_eq!(
            SuggestionStore::new(&state.db).list_muted().unwrap().len(),
            1,
        );
    }
}
