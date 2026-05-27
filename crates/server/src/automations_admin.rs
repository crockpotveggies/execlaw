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
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use crate::automation_runtime::FlowEventInput;
use execlaw_core::automation_runs::{AutomationRunRow, AutomationRunStore};
use execlaw_core::automation_suggestions::SuggestionStore;
use execlaw_core::automations::{
    AutomationDef, AutomationError, AutomationRow, AutomationStore, AutomationUpsert,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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
        // Slice G (2026-05-26) — operator-facing branching hints
        // collected from every installed plugin's
        // `[[branch_suggestions]]` section. The SPA's three
        // Rhai-expression pickers (Branch form, edge.when,
        // trigger.when) fetch the same payload + filter
        // client-side by the current flow's trigger.event_kind.
        .route(
            "/api/admin/automations/branch-suggestions",
            get(list_branch_suggestions),
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

// --- Slice G: plugin-suggested branch dimensions ---------------------
//
// Each installed plugin's manifest can declare one or more
// `[[branch_suggestions]]` entries hinting at natural Rhai split
// dimensions for an event_kind. The endpoint walks every installed
// plugin row, parses the persisted manifest TOML, collects the
// suggestions, and tags each with the source plugin id + version so
// the SPA can badge the dropdown entries ("From slack 0.3.3 ...").
//
// Cost: one `state_plugins` SELECT + N TOML re-parses. For the
// dozen-or-so plugins a typical install carries, this completes in
// well under 5 ms. Not cached — the manifest set changes on every
// install/upgrade and a cache invalidation hook would add plumbing
// without a meaningful win at the call rate (the SPA fetches once
// per flow-edit session).

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct BranchSuggestionDto {
    /// Trigger / event kind this suggestion is meaningful for.
    /// SPA filters by matching the flow's `trigger.kind`.
    pub event_kind: String,
    /// Optional secondary gate — the SPA suppresses the
    /// suggestion when the flow's existing trigger.when does NOT
    /// contain this expression as a substring (string heuristic;
    /// it's a UX gate, not a runtime check).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when_active: Option<String>,
    /// Operator-facing dropdown label.
    pub display_name: String,
    /// One-line hint under the label.
    pub description: String,
    /// Rhai expression with `{placeholder}` chips the operator
    /// fills in.
    pub template: String,
    /// Default value per `{placeholder}` so the suggestion lands
    /// as a working expression.
    pub defaults: std::collections::BTreeMap<String, String>,
    /// Source plugin id (`"slack"`, `"whatsapp"`, ...). The SPA
    /// shows this as a badge in the dropdown so the operator
    /// knows which plugin's payload shape they're branching on.
    pub source_plugin_id: String,
    /// Source plugin version. Helps an operator debug stale
    /// suggestions after an upgrade.
    pub source_plugin_version: String,
}

#[axum::debug_handler]
async fn list_branch_suggestions(
    State(state): State<AppState>,
) -> Result<Json<Vec<BranchSuggestionDto>>, ApiError> {
    let rows = state.plugin_host.list_rows().map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "plugin_list_failed",
        message: format!("{e}"),
    })?;

    let mut out: Vec<BranchSuggestionDto> = Vec::new();
    for row in rows {
        // Skip disabled plugins — their suggestions would be
        // confusing (operator can't reach the events these
        // suggestions discriminate on while the plugin is off).
        // Re-appear on re-enable, no SPA refresh needed beyond
        // the next fetch.
        if !row.enabled {
            continue;
        }
        let manifest = match execlaw_plugin_sdk::PluginManifest::parse(&row.manifest_toml) {
            Ok(m) => m,
            Err(e) => {
                // Shouldn't happen — install path already parsed
                // this manifest. Log + skip rather than fail the
                // whole list (one bad plugin shouldn't blank the
                // dropdown).
                tracing::warn!(
                    plugin_id = %row.plugin_id,
                    error = %e,
                    "branch-suggestions: persisted manifest re-parse failed; skipping plugin"
                );
                continue;
            }
        };
        for s in manifest.branch_suggestions {
            out.push(BranchSuggestionDto {
                event_kind: s.event_kind,
                when_active: s.when_active,
                display_name: s.display_name,
                description: s.description,
                template: s.template,
                defaults: s.defaults,
                source_plugin_id: row.plugin_id.clone(),
                source_plugin_version: row.version.clone(),
            });
        }
    }
    // Deterministic ordering so the SPA dropdown isn't jumpy
    // between fetches. Sort by plugin id then display name; two
    // suggestions from one plugin retain their manifest-file order
    // within that group.
    out.sort_by(|a, b| {
        a.source_plugin_id
            .cmp(&b.source_plugin_id)
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
    Ok(Json(out))
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
    // 2026-05-22 — M6 rip-out: `event_id` mode is gone with the
    // bus. Test-run now only synthesizes. The SPA passes a built
    // envelope when the operator wants to test envelope-gated
    // triggers (origin.kind, identity.trust, etc.); otherwise we
    // default to `system_internal()`.
    let event = match req.sample_event {
        Some(s) => FlowEventInput {
            id: format!("test-run:{}", uuid::Uuid::new_v4()),
            kind: s.kind,
            source: s.source,
            received_at: chrono::Utc::now().timestamp_millis(),
            payload: s.payload,
            internal: false,
            envelope: s
                .envelope
                .unwrap_or_else(execlaw_core::event_envelope::EventEnvelope::system_internal),
        },
        None => {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "test_run_missing_event",
                message: "sample_event is required (event_id mode retired in M6 rip-out)"
                    .into(),
            });
        }
    };
    // The dry-run path holds the calling tokio worker for the
    // duration of the executor (which may invoke the agent pool
    // synchronously via `block_on`). Push it onto a blocking thread
    // so the admin handler doesn't park a runtime worker.
    let db = state.db.clone();
    let plugin_host = Some(state.plugin_host.clone());
    let result = tokio::task::spawn_blocking(move || {
        let ctx = automation_runtime::ExecutorContext::new(db, plugin_host);
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

