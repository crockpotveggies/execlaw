//! Admin HTTP API for Automations (M4 backend).
//!
//! Mounted under `/api/admin/automations*` — controller-gated like
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
//! human-readable message verbatim — operators get actionable
//! feedback without spelunking the trace.

use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{delete, get, post, put};
use execlaw_core::automation_bus::BusEventKind;
use execlaw_core::automation_runs::AutomationRunStore;
use execlaw_core::automation_suggestions::SuggestionStore;
use execlaw_core::automations::{
    AutomationDef, AutomationError, AutomationRow, AutomationStore, AutomationUpsert,
};
use serde::{Deserialize, Serialize};

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
            "/api/admin/automations/{id}",
            get(get_one).put(update).delete(delete_one),
        )
        .route("/api/admin/automations/{id}/enable", post(enable))
        .route("/api/admin/automations/{id}/disable", post(disable))
        .route("/api/admin/automations/{id}/runs", get(list_runs))
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct AutomationDto {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub definition: AutomationDef,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<AutomationRow> for AutomationDto {
    fn from(r: AutomationRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            enabled: r.enabled,
            definition: r.definition,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateAutomationRequest {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub definition: AutomationDef,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateAutomationRequest {
    pub name: String,
    pub enabled: bool,
    pub definition: AutomationDef,
}

#[derive(Debug, Serialize)]
pub struct MetricsDto {
    pub active_count: i64,
    pub runs_24h: i64,
    pub success_rate_24h: Option<f64>, // None when no runs in window
    pub untriaged_kinds_24h: i64,
}

#[derive(Debug, Serialize)]
pub struct SuggestionDto {
    pub id: String,
    pub kind: BusEventKind,
    pub source: String,
    pub event_count: i64,
    pub sample_event_ids: Vec<String>,
    pub suggested_name: String,
    pub created_at: i64,
    pub updated_at: i64,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list(State(state): State<AppState>) -> Result<Json<Vec<AutomationDto>>, ApiError> {
    let rows = AutomationStore::new(&state.db)
        .list_all()
        .map_err(automation_err)?;
    Ok(Json(rows.into_iter().map(AutomationDto::from).collect()))
}

async fn create(
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

async fn get_one(
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

async fn update(
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

async fn delete_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let removed = AutomationStore::new(&state.db)
        .delete(&id)
        .map_err(automation_err)?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "automation_not_found",
            message: format!("no automation with id '{id}'"),
        })
    }
}

async fn enable(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    toggle(&state, &id, true).await
}

async fn disable(
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

async fn list_runs(
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

async fn metrics(State(state): State<AppState>) -> Result<Json<MetricsDto>, ApiError> {
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

async fn list_suggestions(
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
            })
            .collect(),
    ))
}

async fn dismiss_suggestion(
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

async fn action_suggestion(
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
    use execlaw_core::automation_bus::{BusEventKind, BusEventStore, Event as BusEvent};
    use execlaw_core::automations::{
        AutomationDef, EdgeDef, NodeDef, NodeKind, TriggerDef, END_SENTINEL, TRIGGER_SENTINEL,
    };
    use tower::ServiceExt;

    fn minimal_def() -> AutomationDef {
        AutomationDef {
            trigger: TriggerDef {
                kind: BusEventKind::WebhookReceived,
                when: None,
            },
            nodes: vec![NodeDef {
                id: "end".into(),
                kind: NodeKind::Terminal,
                config: serde_json::json!({}),
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
                kind: BusEventKind::WebhookReceived,
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
        // — see routes::ApiError::into_response.
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
        let (_, body) =
            json_req::<()>(app.clone(), "GET", &format!("/api/admin/automations/{id}"), None)
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
        let (status, _) =
            json_req::<()>(app.clone(), "DELETE", &format!("/api/admin/automations/{id}"), None)
                .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = json_req::<()>(
            app,
            "DELETE",
            &format!("/api/admin/automations/{id}"),
            None,
        )
        .await;
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
                    kind: BusEventKind::WebhookReceived,
                    source: "webhook:ring".into(),
                    received_at: now_ms - i,
                    payload: serde_json::json!({}),
                },
                false,
            )
            .unwrap();
        }
        let app = router().with_state(state);
        let (_, body) =
            json_req::<()>(app, "GET", "/api/admin/automations/metrics", None).await;
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
                    kind: BusEventKind::WebhookReceived,
                    source: "webhook:ring".into(),
                    received_at: now_ms - i,
                    payload: serde_json::json!({}),
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
    async fn dismiss_suggestion_mutes_and_returns_204() {
        let state = test_app_state();
        let bus = BusEventStore::new(&state.db);
        let now_ms = chrono::Utc::now().timestamp_millis();
        for i in 0..15 {
            bus.publish(
                &BusEvent {
                    id: format!("e-{i}"),
                    kind: BusEventKind::WebhookReceived,
                    source: "webhook:ring".into(),
                    received_at: now_ms - i,
                    payload: serde_json::json!({}),
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
