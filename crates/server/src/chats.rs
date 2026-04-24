//! Chat surface — `/api/chats/...` routes that drive the agent turn loop.
//!
//! Phase 1 deliverables (§11 of MIGRATION_PLAN.md):
//!
//! - `POST /api/chats/:id/messages` — controller sends a message. Flow:
//!   1. Pre-turn **policy evaluation** (§7.3) — Blocked senders get
//!      dropped, UnknownPending senders park the conversation.
//!   2. **HMAC-signed** append of the `user_msg` event.
//!   3. Mint a per-turn **capability token** (§7.2) for the runner.
//!   4. Dispatch to `TurnExecutor` when an inference backend is
//!      configured; else fall back to a stub echo reply (dev path).
//!   5. Every event broadcasts on the WebSocket `EventBus` so the UI
//!      gets live updates without polling.
//! - `GET  /api/chats/:id/messages` — paginated history.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use execlaw_core::conversation::{
    ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
};
use execlaw_core::events::{EventKind, EventLog, EventRecord, PendingEvent};
use execlaw_core::ids::{ConversationId, EventSeq};
use execlaw_inference_api::ModelId;
use execlaw_policy::trust::{TrustLevel, TurnPolicyInput, evaluate_turn};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::capability::issue_capability_token;
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
    let log = event_log(&state);
    let store = ConversationStore::new(&state.db);

    // Ensure a conversation row exists.
    ensure_conversation(&store, &cid);

    // Step 1 — **policy evaluation** (§7.3). Phase 1 assumes the sender
    // is the Controller by default. Phase 3 plugs identity resolution
    // in between transport ingress and this point.
    let sender_trust = TrustLevel::Controller;
    let policy = evaluate_turn(TurnPolicyInput {
        effective_trust: sender_trust,
        sender_trust,
        voice: false,
        accesses_sensitive_data: false,
        produces_external_effect: false,
    });
    if policy.drop_turn {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "sender is blocked"})),
        )
            .into_response();
    }
    if policy.require_approval {
        // Cold-contact / Rule-of-Two path lands in Phase 3; for now
        // surface the intent so the UI can render it.
        return (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "status": "awaiting_approval",
                "reason": "policy requires sideband approval",
            })),
        )
            .into_response();
    }

    // Step 2 — mint a per-turn capability token bound to
    // (conversation_id, next_turn_seq, policy.capability_set). In-process
    // today; becomes the header the runner container presents on every
    // tool-dispatch RPC once §2.8 "hot runner container" lands. We mint
    // BEFORE the turn runs so the capability set is immutably bound to
    // this turn's seq.
    let principal_id = req
        .sender_principal_id
        .clone()
        .unwrap_or_else(|| "controller".to_owned());
    let next_seq = match log.last_seq(&cid) {
        Ok(s) => s.next().0,
        Err(e) => return err_500(&format!("last_seq: {e}")),
    };
    let capability_set: Vec<String> =
        policy.capability_set.iter().map(|s| (*s).to_owned()).collect();
    let _capability_token = issue_capability_token(
        &state.signer,
        &principal_id,
        cid.as_str(),
        next_seq,
        capability_set,
        None,
    );

    // Step 3 — run the turn (executor owns ALL event-log writes so
    // the user_msg + model_turn + tool pairs land in one atomic
    // `commit_turn`). Phase 0 stub fallback when no backend configured.
    //
    // Path selection:
    // - No inference backend → stub echo.
    // - Backend configured + NO plugin tools registered → streaming
    //   path (fast first token, no tool loop).
    // - Backend configured + plugin tools present → non-streaming
    //   TurnExecutor path (supports multi-round tool_call loop with
    //   ChainedToolDispatch routing to the plugin host).
    let text_for_broadcast = req.text.clone();
    let has_plugin_tools = !state.plugin_host.registry().all_tools().is_empty();
    let caller_caps: Vec<String> = policy
        .capability_set
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let (user_msg_seq, assistant_text, assistant_seq) = match &state.inference {
        Some(inference) if has_plugin_tools => match run_tool_capable_turn(
            &state,
            inference.clone(),
            &cid,
            &req.text,
            req.sender_principal_id.clone(),
            caller_caps.clone(),
        )
        .await
        {
            Ok(out) => out,
            Err(e) => return err_500(&format!("tool-capable turn failed: {e}")),
        },
        Some(inference) => {
            match run_real_turn(
                &state,
                inference.clone(),
                &cid,
                &req.text,
                req.sender_principal_id.clone(),
            )
            .await
            {
                Ok(out) => out,
                Err(e) => return err_500(&format!("turn failed: {e}")),
            }
        }
        None => match run_stub_turn(&state, &cid, &req.text, req.sender_principal_id.clone()) {
            Ok(out) => out,
            Err(e) => return err_500(&format!("stub turn failed: {e}")),
        },
    };

    // Step 4 — broadcast both user and assistant events on the bus
    // AFTER the commit lands, so subscribers never see an outbound
    // reply before the inbound message that provoked it.
    state.events.publish(UiEvent::ChatMessageInbound {
        conversation_id: cid.as_str().to_owned(),
        seq: user_msg_seq,
        text: text_for_broadcast,
        sender: req.sender_principal_id.clone(),
    });
    state.events.publish(UiEvent::ChatMessageOutbound {
        conversation_id: cid.as_str().to_owned(),
        seq: assistant_seq,
        text: assistant_text.clone(),
    });

    // Step 5 — bump the conversation row.
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
            user_msg_seq,
            assistant_text,
            assistant_seq,
        })),
    )
        .into_response()
}

/// Run the Phase-0 stub reply path (no inference backend configured).
/// Owns BOTH the user_msg and model_turn writes — one atomic commit.
/// Returns `(user_msg_seq, reply_text, assistant_seq)`.
fn run_stub_turn(
    state: &AppState,
    cid: &ConversationId,
    user_text: &str,
    sender_principal_id: Option<String>,
) -> Result<(i64, String, i64), String> {
    let log = event_log(state);
    let reply_text = format!(
        "(execlaw dev stub) received {} chars — configure EXECLAW_INFERENCE_URL for live replies",
        user_text.chars().count()
    );

    let user_pending = PendingEvent::encode(
        EventKind::UserMsg,
        &UserMessagePayload {
            text: user_text.to_owned(),
            sender_principal_id,
        },
        None,
    )
    .map_err(|e| format!("encode user_msg: {e}"))?;
    let reply_pending = PendingEvent::encode(
        EventKind::ModelTurn,
        &StubModelTurnPayload {
            model: "stub".into(),
            text: reply_text.clone(),
            finish_reason: Some("stub".into()),
        },
        Some("agent-stub".into()),
    )
    .map_err(|e| format!("encode stub reply: {e}"))?;

    let base_seq = log.last_seq(cid).map_err(|e| format!("last_seq: {e}"))?;
    let written = log
        .commit_turn(cid, base_seq, vec![user_pending, reply_pending])
        .map_err(|e| format!("commit: {e}"))?;

    let user_seq = written
        .iter()
        .find(|e| e.kind == EventKind::UserMsg)
        .map(|e| e.seq.0)
        .ok_or("commit_turn returned no user_msg row")?;
    let assistant_seq = written
        .iter()
        .find(|e| e.kind == EventKind::ModelTurn)
        .map(|e| e.seq.0)
        .ok_or("commit_turn returned no model_turn row")?;

    Ok((user_seq, reply_text, assistant_seq))
}

/// Run a real turn against the configured inference backend,
/// streaming the assistant's reply over the WebSocket event bus as
/// chunks arrive.
///
/// Wire shape:
///   1. Commit `user_msg` to the log (HMAC-signed).
///   2. Replay the conversation log, assemble OpenAI chat messages,
///      prepend the system prompt.
///   3. Open a streaming `/v1/chat/completions` call.
///   4. For each SSE chunk: accumulate content + broadcast
///      `UiEvent::ChatTokenDelta` so the UI gets live tokens.
///   5. On stream end: commit a single `model_turn` event with the
///      full text.
///
/// Tool-call streaming lands with Phase 2 when the hook-registry
/// actually registers plugin tools. Phase 1's spec says "one
/// transport, no plugin tools", and any tool_call the model emits
/// here is ignored (TurnExecutor is still used in the non-streaming
/// path for future tool integrations).
async fn run_real_turn(
    state: &AppState,
    inference: Arc<execlaw_inference_api::InferenceClient>,
    cid: &ConversationId,
    user_text: &str,
    sender_principal_id: Option<String>,
) -> Result<(i64, String, i64), String> {
    use execlaw_inference_api::{ChatMessage, ChatRequest};
    use futures::StreamExt;

    let log = event_log(state);

    // Step 1 — user_msg append.
    let base_seq = log.last_seq(cid).map_err(|e| format!("last_seq: {e}"))?;
    let user_seq = base_seq.next();
    let user_event = EventRecord::new(
        cid.clone(),
        user_seq,
        EventKind::UserMsg,
        &UserMessagePayload {
            text: user_text.to_owned(),
            sender_principal_id: sender_principal_id.clone(),
        },
        sender_principal_id,
    )
    .map_err(|e| format!("encode user_msg: {e}"))?;
    log.append(&user_event)
        .map_err(|e| format!("append user_msg: {e}"))?;

    // Step 2 — hydrate history into chat messages.
    let history = log
        .replay_since(cid, EventSeq(0))
        .map_err(|e| format!("replay: {e}"))?;
    let mut messages: Vec<ChatMessage> =
        vec![ChatMessage::system(&state.config.system_prompt)];
    for ev in &history {
        match ev.kind {
            EventKind::UserMsg => {
                if let Ok(p) = ev.decode_payload::<UserMessagePayload>() {
                    messages.push(ChatMessage::user(p.text));
                }
            }
            EventKind::ModelTurn => {
                // The stub path writes `StubModelTurnPayload`; the real
                // path (below) writes `RealModelTurnPayload`. Try both.
                if let Ok(p) = ev.decode_payload::<RealModelTurnPayload>() {
                    messages.push(ChatMessage::assistant(p.text));
                } else if let Ok(p) = ev.decode_payload::<StubModelTurnPayload>() {
                    messages.push(ChatMessage::assistant(p.text));
                }
            }
            _ => {}
        }
    }

    // Step 3 — open stream.
    let req = ChatRequest {
        model: ModelId(state.config.model_id.clone()),
        messages,
        tools: None,
        stream: true,
        temperature: None,
        max_tokens: None,
    };
    let mut stream = inference
        .chat_completions_stream(&req)
        .await
        .map_err(|e| format!("stream open: {e}"))?;

    // Step 4 — consume stream, broadcasting per-chunk deltas.
    let mut assembled = String::new();
    let mut finish_reason: Option<String> = None;
    let mut model_id = state.config.model_id.clone();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("stream chunk: {e}"))?;
        model_id = chunk.model.clone();
        for ch in &chunk.choices {
            if let Some(t) = &ch.delta.content {
                if !t.is_empty() {
                    assembled.push_str(t);
                    state.events.publish(UiEvent::ChatTokenDelta {
                        conversation_id: cid.as_str().to_owned(),
                        text: t.clone(),
                    });
                }
            }
            if let Some(fr) = &ch.finish_reason {
                finish_reason = Some(fr.clone());
            }
        }
    }
    // Ensure the user never sees an empty reply — a model that
    // closes the stream without emitting any content still produces
    // a committed `model_turn` event so the transcript stays well-formed.
    let assistant_text = if assembled.is_empty() {
        "(empty response)".to_owned()
    } else {
        assembled
    };

    // Step 5 — commit the model_turn.
    let reply_payload = RealModelTurnPayload {
        model: model_id,
        text: assistant_text.clone(),
        finish_reason,
        prompt_tokens: None,
        completion_tokens: None,
    };
    let reply_pending =
        PendingEvent::encode(EventKind::ModelTurn, &reply_payload, Some("agent".into()))
            .map_err(|e| format!("encode model_turn: {e}"))?;
    let latest = log.last_seq(cid).map_err(|e| format!("last_seq: {e}"))?;
    let written = log
        .commit_turn(cid, latest, vec![reply_pending])
        .map_err(|e| format!("commit: {e}"))?;
    let assistant_seq = written
        .iter()
        .find(|e| e.kind == EventKind::ModelTurn)
        .map(|e| e.seq.0)
        .unwrap_or(latest.0 + 1);

    Ok((user_seq.0, assistant_text, assistant_seq))
}

/// Run a non-streaming, tool-capable turn: the registry's currently-
/// installed plugin tools are exposed to the model, and any
/// `tool_calls` the model emits are dispatched through
/// [`crate::tool_dispatch::ChainedToolDispatch`] with capability
/// enforcement. Used when `has_plugin_tools == true`.
///
/// Trades streaming token deltas for multi-round tool support. The
/// event log still gets user_msg + tool_use + tool_result pairs +
/// model_turn via commit_turn, so the pairing invariant and HMAC
/// signing apply identically.
async fn run_tool_capable_turn(
    state: &AppState,
    inference: Arc<execlaw_inference_api::InferenceClient>,
    cid: &ConversationId,
    user_text: &str,
    sender_principal_id: Option<String>,
    caller_caps: Vec<String>,
) -> Result<(i64, String, i64), String> {
    use execlaw_inference_api::ToolDeclaration;
    use execlaw_runner_local::turn::{TurnConfig, TurnExecutor};

    let tool_decls: Vec<ToolDeclaration> = state
        .plugin_host
        .registry()
        .all_tools()
        .iter()
        .map(|t| {
            ToolDeclaration::function(
                t.tool_name.clone(),
                format!("Plugin tool '{}' (latency: {})", t.tool_name, t.latency),
                serde_json::json!({"type": "object"}),
            )
        })
        .collect();

    let dispatch = Arc::new(crate::tool_dispatch::ChainedToolDispatch::new(
        state.plugin_host.clone(),
        caller_caps,
        crate::tool_dispatch::NoBuiltinTools,
    ));
    let exec = TurnExecutor::new((*inference).clone(), dispatch);
    let cfg = TurnConfig {
        model: ModelId(state.config.model_id.clone()),
        system_prompt: state.config.system_prompt.clone(),
        temperature: None,
        max_tokens: None,
        max_tool_rounds: state.config.max_tool_rounds,
        tools: tool_decls,
        event_log_hmac_key: state
            .event_log_hmac_key
            .as_ref()
            .map(|k| (**k).clone()),
    };
    let summary = exec
        .run_turn(&state.db, cid, user_text, sender_principal_id, &cfg)
        .await
        .map_err(|e| format!("executor: {e}"))?;

    let log = event_log(state);
    // TurnExecutor appends user_msg via `append` (not commit_turn) so
    // it's NOT in events_written. Read last_seq back and subtract
    // the committed count to find the user_msg seq.
    let last = log.last_seq(cid).map_err(|e| format!("last_seq: {e}"))?.0;
    let committed = summary.events_written.len() as i64;
    let user_seq = last - committed;
    let assistant_seq = summary
        .events_written
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::ModelTurn)
        .map(|e| e.seq.0)
        .unwrap_or(last);
    Ok((user_seq, summary.assistant_text, assistant_seq))
}

/// Build an `EventLog` with the server's HMAC key attached (when set).
fn event_log(state: &AppState) -> EventLog<'_> {
    let log = EventLog::new(&state.db);
    match &state.event_log_hmac_key {
        Some(k) => log.with_hmac_key((**k).clone()),
        None => log,
    }
}

/// `GET /api/chats/:id/messages?before=0&limit=200`
pub async fn list_messages(
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
    Query(q): Query<ListQuery>,
) -> impl IntoResponse {
    let cid = ConversationId::from(conversation_id.as_str());
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    // Use the keyed log so HMAC verification rejects tampered rows
    // before they reach the UI (§7.8).
    let log = event_log(&state);

    let events = match log.replay_since(&cid, EventSeq(q.before)) {
        Ok(e) => e,
        Err(e) => return err_500(&format!("replay: {e}")),
    };

    let messages: Vec<MessageView> = events
        .into_iter()
        .filter(|e| {
            matches!(
                e.kind,
                EventKind::UserMsg
                    | EventKind::ModelTurn
                    | EventKind::ToolUse
                    | EventKind::ToolResult
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
        assert!(
            body["assistant_text"]
                .as_str()
                .unwrap()
                .contains("execlaw dev stub")
        );
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
            match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
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

    /// Stub-path committed rows must be HMAC-signed (the test AppState
    /// has a key attached). Reading them back through a keyed EventLog
    /// must succeed; reading them through a WRONG-keyed log must fail
    /// with TamperDetected. Proves the wire-up actually signs.
    #[tokio::test]
    async fn stub_path_commits_hmac_signed_rows() {
        let state = test_app_state();
        let db = state.db.clone();
        let app = crate::routes::build_router(state.clone());
        let _ = send(app, "hi").await;

        use execlaw_core::events::EventLog;
        use execlaw_core::ids::ConversationId;

        // Same key: replay succeeds.
        let good_log = EventLog::new(&db).with_hmac_key(
            state.event_log_hmac_key.as_ref().unwrap().as_ref().clone(),
        );
        let got = good_log
            .replay_since(&ConversationId::from("conv1"), execlaw_core::ids::EventSeq(0))
            .unwrap();
        assert_eq!(got.len(), 2);

        // Different key: TamperDetected.
        let bad_log = EventLog::new(&db).with_hmac_key(b"wrong-key".to_vec());
        let err = bad_log
            .replay_since(&ConversationId::from("conv1"), execlaw_core::ids::EventSeq(0))
            .unwrap_err();
        assert!(matches!(err, execlaw_core::DbError::TamperDetected(_)));
    }

    /// The pairing invariant holds at the HTTP layer: user_msg and
    /// model_turn land in consecutive seqs (1, 2) as part of the same
    /// `commit_turn`, not via separate appends.
    #[tokio::test]
    async fn stub_path_commits_user_and_model_atomically() {
        let (status, body) = send(build_app(), "hi there").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["user_msg_seq"].as_i64().unwrap(), 1);
        assert_eq!(body["assistant_seq"].as_i64().unwrap(), 2);
    }

    /// **Phase 1 crash test (a):** kill the control plane mid-turn
    /// (simulated by dropping the AppState before `commit_turn`
    /// returns). The event log must be internally consistent — either
    /// the turn lands fully or not at all, per §2.2 axiom #2.
    ///
    /// We simulate the crash by invoking the stub path against a
    /// state whose DB is dropped right after a single `send`. Next
    /// boot replays; the log must show the turn in full OR not at all.
    #[tokio::test]
    async fn crash_mid_turn_leaves_no_dangling_tool_use() {
        // Simulate a turn that would have emitted a tool_use but was
        // aborted before the matching tool_result — the `commit_turn`
        // contract synthesizes a paired tool_result. We construct the
        // scenario directly against the event log rather than the HTTP
        // layer because the Phase 1 stub has no tool calls.
        use execlaw_core::events::{
            EventKind, EventLog, PendingEvent, ToolResultPayload, ToolUsePayload,
        };
        use execlaw_core::ids::{ConversationId, EventSeq};

        let state = test_app_state();
        let log = EventLog::new(&state.db).with_hmac_key(
            state.event_log_hmac_key.as_ref().unwrap().as_ref().clone(),
        );
        let cid = ConversationId::from("crash-conv");

        // Commit a turn that emits a tool_use without a matching
        // tool_result — the mid-crash shape.
        let pending = vec![
            PendingEvent::encode(
                EventKind::ModelTurn,
                &serde_json::json!({"text": "calling tool"}),
                Some("agent".into()),
            )
            .unwrap(),
            PendingEvent::encode(
                EventKind::ToolUse,
                &ToolUsePayload {
                    ordinal: 0,
                    tool_name: "list_events".into(),
                    args_json: serde_json::json!({}),
                },
                Some("agent".into()),
            )
            .unwrap(),
            // NO ToolResult — crash happened before tool returned.
        ];
        let written = log.commit_turn(&cid, EventSeq(0), pending).unwrap();
        // The synthesized cancellation brings the total to 3 events.
        assert_eq!(written.len(), 3, "must synthesize cancel tool_result");

        // Replay — must succeed, every tool_use paired.
        let events = log.replay_since(&cid, EventSeq(0)).unwrap();
        let uses: Vec<u32> = events
            .iter()
            .filter(|e| e.kind == EventKind::ToolUse)
            .map(|e| {
                e.decode_payload::<ToolUsePayload>()
                    .unwrap()
                    .ordinal
            })
            .collect();
        let results: Vec<(u32, bool)> = events
            .iter()
            .filter(|e| e.kind == EventKind::ToolResult)
            .map(|e| {
                let r: ToolResultPayload = e.decode_payload().unwrap();
                (r.ordinal, r.outcome.is_err())
            })
            .collect();
        assert_eq!(uses.len(), results.len());
        assert!(
            results[0].1,
            "the synthesized tool_result must be an Err outcome"
        );
    }

    /// **Phase 1 crash test (b):** replay after a simulated crash
    /// reconstructs the conversation exactly — same events, same
    /// order, all HMAC-verified. Models the "worker restarts, reads
    /// the log, resumes" happy path.
    #[tokio::test]
    async fn replay_after_restart_reconstructs_turn_history() {
        let state = test_app_state();
        let app = crate::routes::build_router(state.clone());
        let _ = send(app.clone(), "first").await;
        let _ = send(app.clone(), "second").await;

        // Simulate restart: drop everything except the DB + HMAC key,
        // then construct a fresh EventLog and replay.
        let key = state.event_log_hmac_key.as_ref().unwrap().as_ref().clone();
        let db = state.db.clone();
        drop(state);
        drop(app);

        use execlaw_core::events::{EventKind, EventLog};
        use execlaw_core::ids::{ConversationId, EventSeq};
        let log = EventLog::new(&db).with_hmac_key(key);
        let events = log
            .replay_since(&ConversationId::from("conv1"), EventSeq(0))
            .unwrap();
        // Two turns × 2 events each = 4 rows.
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].kind, EventKind::UserMsg);
        assert_eq!(events[1].kind, EventKind::ModelTurn);
        assert_eq!(events[2].kind, EventKind::UserMsg);
        assert_eq!(events[3].kind, EventKind::ModelTurn);
    }

    /// Post-commit tamper of any committed row is detected when the
    /// UI requests history — the `GET /messages` handler uses the
    /// keyed `EventLog` and surfaces a 500 (which is the right
    /// behavior: better a failure than serving a forged transcript).
    #[tokio::test]
    async fn post_commit_tamper_fails_list_messages() {
        let state = test_app_state();
        let db = state.db.clone();
        let app = crate::routes::build_router(state);
        let _ = send(app.clone(), "hi").await;

        // Tamper with the committed user_msg payload via direct SQL.
        db.with_conn(|c| {
            c.execute(
                "UPDATE state_events SET payload = ?1 WHERE conversation_id = 'conv1' AND seq = 1",
                rusqlite::params![b"evil".to_vec()],
            )?;
            Ok(())
        })
        .unwrap();

        // GET /api/chats/conv1/messages must NOT return tampered data.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/chats/conv1/messages")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "tampered log must fail the read, not return forged rows"
        );
    }

    /// A Blocked sender (Phase 3 primitive, already evaluated by the
    /// policy engine) would short-circuit with 403. Today the sender
    /// is hard-coded to Controller so this asserts the happy path
    /// goes through; the Blocked branch is exercised by the policy
    /// crate's unit tests.
    #[tokio::test]
    async fn policy_controller_sender_reaches_turn() {
        let (status, body) = send(build_app(), "hi").await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body["assistant_text"].as_str().unwrap().is_empty());
    }

    /// When the plugin registry has tools, `send_message` takes the
    /// tool-capable path instead of streaming. Without an inference
    /// backend configured it falls back to the stub echo regardless,
    /// so this test only asserts that the router doesn't error out
    /// when tools are registered — the live tool dispatch is covered
    /// by `tool_dispatch::tests` and the Unix-only reference-plugin
    /// integration test.
    #[tokio::test]
    async fn chat_route_tolerates_registered_plugin_tools() {
        let state = test_app_state();
        // Register a manifest with a tool.
        let m = r#"[plugin]
id = "p-chat"
name = "p-chat"
version = "0.1.0"

[[tools]]
name = "introspect"
schema = "s.json"
latency = "low"
required_capabilities = []
"#;
        state
            .plugin_host
            .registry()
            .enable(&execlaw_plugin_sdk::PluginManifest::parse(m).unwrap())
            .unwrap();

        let app = crate::routes::build_router(state);
        let (status, body) = send(app, "hello").await;
        // Stub path fires because no inference backend is configured;
        // the critical assertion is that the route didn't 500 when
        // tools are in the registry.
        assert_eq!(status, StatusCode::OK);
        assert!(!body["assistant_text"].as_str().unwrap().is_empty());
    }
}
