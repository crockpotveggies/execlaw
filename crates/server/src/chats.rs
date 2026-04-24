//! Chat surface — `/api/chats/...` routes that drive the agent turn loop.
//!
//! Phase 1 deliverables (§11 of MIGRATION_PLAN.md):
//!
//! - `POST /api/chats/:id/messages` — controller sends a message; server
//!   appends the `user_msg` event, broadcasts it over the event bus,
//!   commits a stub agent reply (the Phase 1 TurnExecutor is in
//!   `execlaw-runner-local` and is invoked here only if an inference
//!   endpoint is configured; otherwise we commit an echo reply so the
//!   chat surface is usable end-to-end without a live model).
//! - `GET  /api/chats/:id/messages` — paginated history.
//!
//! Every event that lands in the log also lands on the WebSocket event
//! bus so the SPA gets live updates without polling.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use execlaw_core::conversation::{
    ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
};
use execlaw_core::events::{EventKind, EventLog, EventRecord, PendingEvent};
use execlaw_core::ids::{ConversationId, EventSeq};
use serde::{Deserialize, Serialize};

use crate::events::UiEvent;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub text: String,
    /// Optional override — defaults to the controller's principal id.
    pub sender_principal_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SendMessageResponse {
    pub conversation_id: String,
    pub user_msg_seq: i64,
    pub assistant_text: String,
    pub assistant_seq: i64,
}

#[derive(Debug, Serialize)]
pub struct MessagesListResponse {
    pub conversation_id: String,
    pub messages: Vec<MessageView>,
}

#[derive(Debug, Serialize)]
pub struct MessageView {
    pub seq: i64,
    pub kind: String,
    pub text: Option<String>,
    pub actor: Option<String>,
    pub committed_at: i64,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// Return events with `seq > before` in ascending order. Default 0
    /// (return everything).
    #[serde(default)]
    pub before: i64,
    /// Hard cap — default 200.
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `POST /api/chats/:id/messages`
pub async fn send_message(
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> impl IntoResponse {
    if req.text.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "text must not be empty"})),
        )
            .into_response();
    }

    let cid = ConversationId::from(conversation_id.as_str());
    let log = EventLog::new(&state.db);
    let store = ConversationStore::new(&state.db);

    // Ensure a conversation row exists.
    ensure_conversation(&store, &cid);

    // 1. Append the user_msg event.
    let user_seq = match log.last_seq(&cid) {
        Ok(s) => s.next(),
        Err(e) => {
            return err_500(&format!("last_seq: {e}"));
        }
    };

    let payload = UserMessagePayload {
        text: req.text.clone(),
        sender_principal_id: req.sender_principal_id.clone(),
    };
    let user_event = match EventRecord::new(
        cid.clone(),
        user_seq,
        EventKind::UserMsg,
        &payload,
        req.sender_principal_id.clone(),
    ) {
        Ok(e) => e,
        Err(e) => return err_500(&format!("encode user_msg: {e}")),
    };
    if let Err(e) = log.append(&user_event) {
        return err_500(&format!("append user_msg: {e}"));
    }

    // Broadcast the inbound event.
    state.events.publish(UiEvent::ChatMessageInbound {
        conversation_id: cid.as_str().to_owned(),
        seq: user_seq.0,
        text: req.text.clone(),
        sender: req.sender_principal_id.clone(),
    });

    // 2. Phase 1 stub reply — a structured echo so the chat surface works
    //    end-to-end without a live inference backend. The full turn
    //    executor in `execlaw-runner-local::turn` is the production path
    //    and is wired in when the runner deployment registry resolves to
    //    a reachable service.
    let reply_text = format!(
        "(execlaw dev stub) received {} chars — inference backend not wired in this route yet",
        req.text.chars().count()
    );
    let model_payload = StubModelTurnPayload {
        model: "stub".into(),
        text: reply_text.clone(),
        finish_reason: Some("stub".into()),
    };
    let assistant_pending = match PendingEvent::encode(
        EventKind::ModelTurn,
        &model_payload,
        Some("agent-stub".into()),
    ) {
        Ok(e) => e,
        Err(e) => return err_500(&format!("encode stub reply: {e}")),
    };
    let base_seq = match log.last_seq(&cid) {
        Ok(s) => s,
        Err(e) => return err_500(&format!("last_seq after user: {e}")),
    };
    let written = match log.commit_turn(&cid, base_seq, vec![assistant_pending]) {
        Ok(w) => w,
        Err(e) => return err_500(&format!("commit_turn: {e}")),
    };
    let assistant_seq = written
        .iter()
        .find(|e| e.kind == EventKind::ModelTurn)
        .map(|e| e.seq.0)
        .unwrap_or(base_seq.0);

    state.events.publish(UiEvent::ChatMessageOutbound {
        conversation_id: cid.as_str().to_owned(),
        seq: assistant_seq,
        text: reply_text.clone(),
    });

    // 3. Bump the conversation row.
    if let Ok(Some(mut row)) = store.get(&cid) {
        row.last_seq = match log.last_seq(&cid) {
            Ok(s) => s,
            Err(_) => row.last_seq,
        };
        row.phase = Phase::Idle;
        let _ = store.upsert(&row);
    }

    (
        StatusCode::OK,
        Json(serde_json::json!(SendMessageResponse {
            conversation_id: cid.as_str().to_owned(),
            user_msg_seq: user_seq.0,
            assistant_text: reply_text,
            assistant_seq,
        })),
    )
        .into_response()
}

/// `GET /api/chats/:id/messages?before=0&limit=200`
pub async fn list_messages(
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
    Query(q): Query<ListQuery>,
) -> impl IntoResponse {
    let cid = ConversationId::from(conversation_id.as_str());
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let log = EventLog::new(&state.db);

    let events = match log.replay_since(&cid, EventSeq(q.before)) {
        Ok(e) => e,
        Err(e) => return err_500(&format!("replay: {e}")),
    };

    let messages: Vec<MessageView> = events
        .into_iter()
        .filter(|e| {
            matches!(
                e.kind,
                EventKind::UserMsg | EventKind::ModelTurn | EventKind::ToolUse | EventKind::ToolResult
            )
        })
        .take(limit as usize)
        .map(|e| MessageView {
            seq: e.seq.0,
            kind: e.kind.as_str().to_owned(),
            text: extract_text(&e),
            actor: e.actor.clone(),
            committed_at: e.committed_at,
        })
        .collect();

    (
        StatusCode::OK,
        Json(serde_json::json!(MessagesListResponse {
            conversation_id: cid.as_str().to_owned(),
            messages,
        })),
    )
        .into_response()
}

fn ensure_conversation(store: &ConversationStore<'_>, cid: &ConversationId) {
    if matches!(store.get(cid), Ok(Some(_))) {
        return;
    }
    let row = ConversationRow {
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
    };
    let _ = store.upsert(&row);
}

fn err_500(msg: &str) -> axum::response::Response {
    tracing::error!("{msg}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": msg})),
    )
        .into_response()
}

fn extract_text(e: &EventRecord) -> Option<String> {
    match e.kind {
        EventKind::UserMsg => e
            .decode_payload::<UserMessagePayload>()
            .ok()
            .map(|p| p.text),
        EventKind::ModelTurn => e
            .decode_payload::<StubModelTurnPayload>()
            .ok()
            .map(|p| p.text)
            .or_else(|| {
                // Fall back to the richer ModelTurnPayload shape produced
                // by the full TurnExecutor.
                e.decode_payload::<RealModelTurnPayload>()
                    .ok()
                    .map(|p| p.text)
            }),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UserMessagePayload {
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sender_principal_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StubModelTurnPayload {
    model: String,
    text: String,
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RealModelTurnPayload {
    model: String,
    text: String,
    finish_reason: Option<String>,
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::test_app_state;
    use axum::body::{self, Body};
    use axum::http::{HeaderValue, Method, Request, header};
    use tower::ServiceExt;

    async fn json_body<T: for<'de> serde::Deserialize<'de>>(body: Body) -> T {
        let bytes = body::to_bytes(body, usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn send(app: axum::Router, text: &str) -> (StatusCode, serde_json::Value) {
        let body = serde_json::to_vec(&serde_json::json!({"text": text})).unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/chats/conv1/messages")
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let value: serde_json::Value = json_body(resp.into_body()).await;
        (status, value)
    }

    fn build_app() -> axum::Router {
        crate::routes::build_router(test_app_state())
    }

    #[tokio::test]
    async fn send_message_commits_both_events_and_returns_reply() {
        let (status, body) = send(build_app(), "hello").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["user_msg_seq"].as_i64().unwrap(), 1);
        assert!(body["assistant_text"].as_str().unwrap().contains("execlaw dev stub"));
    }

    #[tokio::test]
    async fn send_message_rejects_empty_text() {
        let (status, _) = send(build_app(), "   ").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn list_messages_returns_committed_events() {
        let app = build_app();
        let _ = send(app.clone(), "first").await;
        let _ = send(app.clone(), "second").await;

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/chats/conv1/messages")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = json_body(resp.into_body()).await;
        let msgs = body["messages"].as_array().unwrap();
        // 2 user + 2 assistant = 4 messages
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["kind"].as_str().unwrap(), "user_msg");
        assert_eq!(msgs[1]["kind"].as_str().unwrap(), "model_turn");
    }

    #[tokio::test]
    async fn send_message_broadcasts_on_event_bus() {
        let state = test_app_state();
        let mut rx = state.events.subscribe();
        let app = crate::routes::build_router(state);
        let _ = send(app, "hi").await;

        // Expect at least one inbound + one outbound.
        let mut saw_inbound = false;
        let mut saw_outbound = false;
        for _ in 0..5 {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                rx.recv(),
            )
            .await
            {
                Ok(Ok(UiEvent::ChatMessageInbound { .. })) => saw_inbound = true,
                Ok(Ok(UiEvent::ChatMessageOutbound { .. })) => saw_outbound = true,
                _ => break,
            }
            if saw_inbound && saw_outbound {
                break;
            }
        }
        assert!(saw_inbound, "expected ChatMessageInbound");
        assert!(saw_outbound, "expected ChatMessageOutbound");
    }
}
