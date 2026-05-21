//! Web prompt entrypoint + flow-run SSE subscription (M6 slice 7).
//!
//! Two HTTP surfaces:
//!
//! * `POST /api/web/prompt` — operator types in the SPA chat input,
//!   we publish a `web.prompt.submitted` event onto the bus with
//!   `OriginRef::WebSocketSession`. The bus matcher picks up the
//!   event and runs the default web flow (or operator-authored
//!   overrides).
//!
//! * `GET /api/automations/flow-runs/{run_id}/events` — Server-Sent
//!   Events stream subscribing to a run's `FlowChannelHub` channel.
//!   The SPA opens an EventSource on each new run to render the
//!   live trace. Auto-closes on `RunFinished`.
//!
//! Shadow mode: this endpoint does NOT replace the existing chat
//! `send_message` path. Both run in parallel for the same prompt
//! until we verify parity (slice 9 wires the comparison UI).

use crate::flow_channel::FlowChannelEvent;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use execlaw_core::automation_bus::{BusEventKind, Event as BusEvent};
use execlaw_core::event_envelope::{EventEnvelope, OriginRef, SenderIdentity};
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::pin::Pin;
use std::time::Duration;
use utoipa::ToSchema;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/web/prompt", post(submit_prompt))
        .route(
            "/api/automations/flow-runs/{run_id}/events",
            get(subscribe_events),
        )
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SubmitPromptRequest {
    /// The operator's typed text.
    pub text: String,
    /// Conversation to attach to. Required — the reply lands here
    /// via `OriginRef::ChatAppend` so the SPA's existing per-
    /// conversation WebSocket subscription receives the
    /// `UiEvent::ChatMessageOutbound` broadcast and the message
    /// persists into `state_events`.
    pub conversation_id: String,
    /// Existing attachment_ids the operator wants to bundle.
    #[serde(default)]
    pub attachment_ids: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SubmitPromptResponse {
    /// Bus event id. The SPA uses this as a correlation handle to
    /// look up which automation run was triggered (events that
    /// match no operator flow + run the default still attach to a
    /// run_id observable via `/automations/runs?correlation_id=…`).
    pub event_id: String,
}

#[axum::debug_handler]
pub async fn submit_prompt(
    State(state): State<AppState>,
    Json(req): Json<SubmitPromptRequest>,
) -> Result<Json<SubmitPromptResponse>, ApiError> {
    if req.text.trim().is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "empty_prompt",
            message: "prompt text is empty".into(),
        });
    }
    let event_id = format!("web-prompt-{}", uuid::Uuid::new_v4());
    let payload = serde_json::json!({
        "text": req.text,
        "conversation_id": req.conversation_id,
        "attachment_ids": req.attachment_ids,
    });
    // Build the envelope. Reply target = ChatAppend keyed by the
    // SPA's conversation id; the chat_append handler persists the
    // model_turn into the event log AND broadcasts
    // UiEvent::ChatMessageOutbound on the existing per-conversation
    // WebSocket subscription. Identity defaults to System for now;
    // slice C resolves the originating operator principal.
    let envelope = EventEnvelope {
        origin: OriginRef::ChatAppend {
            conversation_id: req.conversation_id.clone(),
        },
        identity: SenderIdentity::System,
        correlation_id: event_id.clone(),
        parent_event_id: None,
    };
    let evt = BusEvent {
        id: event_id.clone(),
        kind: BusEventKind::Other, // until we widen BusEventKind to free-form strings (M6.5)
        source: "core:web".into(),
        received_at: chrono::Utc::now().timestamp_millis(),
        payload,
        envelope: Some(envelope),
    };
    state
        .automation_bus
        .publish(evt)
        .await
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "bus_publish_failed",
            message: format!("{e}"),
        })?;
    Ok(Json(SubmitPromptResponse { event_id }))
}

/// SSE subscription. Stream emits one `data:` frame per
/// `FlowChannelEvent` until the channel closes (sender dropped) or
/// a `RunFinished` event arrives.
pub async fn subscribe_events(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let rx = state.flow_channel.subscribe(&run_id).await;
    // Manual unfold over the broadcast receiver — avoids adding a
    // `tokio-stream` dep just for `BroadcastStream`.
    let strm = stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let json = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
                    let mut e = Event::default().data(json);
                    if let Some(name) = event_kind_str(&ev) {
                        e = e.event(name);
                    }
                    return Some((Ok::<_, Infallible>(e), rx));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    let strm: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = Box::pin(strm);
    Sse::new(strm)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

fn event_kind_str(ev: &FlowChannelEvent) -> Option<&'static str> {
    Some(match ev {
        FlowChannelEvent::NodeStarted { .. } => "node_started",
        FlowChannelEvent::NodeFinished { .. } => "node_finished",
        FlowChannelEvent::AgentTurnStarted { .. } => "agent_turn_started",
        FlowChannelEvent::AgentTextDelta { .. } => "agent_text_delta",
        FlowChannelEvent::AgentToolCallDelta { .. } => "agent_tool_call_delta",
        FlowChannelEvent::AgentTurnFinished { .. } => "agent_turn_finished",
        FlowChannelEvent::ReplyRouted { .. } => "reply_routed",
        FlowChannelEvent::RunFinished { .. } => "run_finished",
    })
}

/// Default web-prompt flow shipped by core. Imported into
/// `state_automations` on boot when the registry is fresh; existing
/// installs see the row added on first run of an updated build.
/// Disabled by default in shadow mode so the SPA's existing chat
/// path keeps working until the operator opts in.
pub fn default_web_flow_json() -> serde_json::Value {
    serde_json::json!({
        "trigger": {"kind": "other", "when": null},
        "nodes": [
            {
                "id": "ask",
                "kind": "AskAgent",
                "config": {
                    "prompt": "{{event.payload.text}}",
                    "attachments": [],
                    "exit_tools": [
                        {
                            "name": "respond",
                            "description": "Final reply to the user. `text` is rendered as the chat message body.",
                            "args_schema": {
                                "type": "object",
                                "properties": {
                                    "text": {"type": "string"}
                                },
                                "required": ["text"]
                            }
                        }
                    ]
                }
            },
            {
                "id": "reply",
                "kind": "SendReply",
                "config": {
                    "source": "from_agent",
                    "from_node": "ask"
                }
            },
            {
                "id": "end",
                "kind": "Terminal",
                "config": {}
            }
        ],
        "edges": [
            {"from": "trigger", "to": "ask", "when": null},
            {"from": "ask", "to": "reply", "when": null},
            {"from": "reply", "to": "end", "when": null}
        ]
    })
}

/// Seed the core default web prompt flow into `state_automations`
/// if no row claims the slot yet. Idempotent. Stamps `source = 'core'`
/// so the row is protected from operator deletion via the regular
/// admin endpoint (the install lifecycle owns these rows).
pub fn ensure_default_web_flow(db: &execlaw_core::Database) -> Result<(), String> {
    use execlaw_core::automations::{AutomationStore, AutomationUpsert};

    let store = AutomationStore::new(db);
    let rows = store.list_all().map_err(|e| format!("list: {e}"))?;
    const NAME: &str = "Default web prompt flow";
    if rows.iter().any(|r| r.name == NAME) {
        return Ok(());
    }
    let def: execlaw_core::automations::AutomationDef =
        serde_json::from_value(default_web_flow_json())
            .map_err(|e| format!("default flow parse: {e}"))?;
    let now = chrono::Utc::now().timestamp_millis();
    let row = store
        .upsert(
            &AutomationUpsert {
                id: None,
                name: NAME.into(),
                enabled: true,
                definition: def,
            },
            now,
        )
        .map_err(|e| format!("upsert default flow: {e}"))?;
    // Flip the source column so the row reads as a core default —
    // delete is refused, the SPA hides the delete button. Direct
    // SQL because `AutomationUpsert` intentionally doesn't expose
    // the source column to operator-driven writes.
    db.with_conn(|c| {
        c.execute(
            "UPDATE state_automations SET source = 'core' WHERE id = ?1",
            rusqlite::params![row.id],
        )?;
        Ok(())
    })
    .map_err(|e| format!("stamp source=core: {e}"))?;
    tracing::info!("M6: seeded default web prompt flow (enabled, source=core)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_web_flow_json_parses_as_automation_def() {
        let v = default_web_flow_json();
        let def: execlaw_core::automations::AutomationDef =
            serde_json::from_value(v).expect("default flow must parse");
        assert_eq!(def.nodes.len(), 3);
        assert_eq!(def.edges.len(), 3);
        assert!(def.nodes.iter().any(|n| n.id == "ask"));
        assert!(def.nodes.iter().any(|n| n.id == "reply"));
        assert!(def.nodes.iter().any(|n| n.id == "end"));
    }

    #[test]
    fn default_web_flow_passes_validator() {
        let v = default_web_flow_json();
        let def: execlaw_core::automations::AutomationDef =
            serde_json::from_value(v).unwrap();
        execlaw_core::automations::validate(&def).expect("default flow must validate clean");
    }
}
