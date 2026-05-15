//! Turn execution — the core loop that drives one agent turn.
//!
//! This is the Phase 1 implementation of the turn pattern described in
//! MIGRATION_PLAN §2.4 "The turn as a transaction". A turn:
//!
//! 1. Pulls the conversation's event log to assemble prompt context.
//! 2. Calls the inference backend via [`execlaw_inference_api`].
//! 3. If the model emitted `tool_calls`, dispatches each via the
//!    [`ToolDispatch`] trait, collecting `tool_result`s.
//! 4. Commits the whole turn in ONE SQLite transaction via
//!    [`execlaw_core::events::EventLog::commit_turn`], which enforces the
//!    `tool_use`/`tool_result` pairing invariant.
//!
//! External side-effecting tools must enqueue outbox rows (not dispatch
//! directly) so delivery happens out-of-band through the outbox relay.
//!
//! This skeleton wires the plumbing; richer features (sub-agent spawn,
//! compaction, planner/executor split for untrusted turns, voice pipeline
//! integration) land incrementally on top of this shape.

use async_trait::async_trait;
use execlaw_core::conversation::{ConversationStore, Phase};
use execlaw_core::db::Database;
use execlaw_core::events::{
    EventKind, EventLog, EventRecord, PendingEvent, ToolResultPayload, ToolUsePayload,
};
use execlaw_core::ids::{ConversationId, EventSeq};
use execlaw_inference_api::{
    ChatMessage, ChatRequest, InferenceClient, InferenceError, ModelId, Role, ToolCall,
    ToolDeclaration,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Phase observer
// ---------------------------------------------------------------------------

/// Hook the runner calls at FSM-phase boundaries during a turn. The
/// runner-local crate stays agnostic of the server's event bus —
/// implementations are wired by the caller.
///
/// Today the runner emits two transitions:
///   * `Phase::AwaitingTool` immediately before dispatching a tool
///     call (so transports can keep the typing indicator on through
///     the tool round).
///   * `Phase::Thinking` immediately after a tool round completes,
///     when the next LLM call starts.
///
/// `Phase::Idle` is the responsibility of the *caller* (`chats.rs`),
/// since it knows when the entire send-message pipeline has finished
/// — including the parts that aren't the runner's concern (audit
/// log, message broadcast, conversation row bump).
pub trait PhaseObserver: Send + Sync {
    fn observe(&self, phase: Phase);
}

// ---------------------------------------------------------------------------
// Tool dispatch trait
// ---------------------------------------------------------------------------

/// Handles one named tool. Returns either a success JSON value or a
/// cancellation reason string. The caller wraps both into a
/// [`ToolResultPayload`].
#[async_trait]
pub trait ToolDispatch: Send + Sync {
    async fn call(
        &self,
        tool_name: &str,
        args_json: &serde_json::Value,
    ) -> Result<serde_json::Value, String>;
}

// ---------------------------------------------------------------------------
// Per-turn input + output
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessagePayload {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_principal_id: Option<String>,
    /// Originating transport (signal / email / voice / sms) when
    /// this user_msg arrived from a transport bridge. None for the
    /// default web path. Surfaced to the SPA so it can render a
    /// per-message channel icon in the chat view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_origin: Option<String>,
    /// 2026-05-15 — IDs into `state_attachments` for images the
    /// operator/contact attached to this turn. Must match the
    /// field name + shape on the server-side `UserMessagePayload`
    /// in `crates/server/src/chats.rs` so a turn written by either
    /// code path round-trips consistently when replayed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelTurnPayload {
    pub model: String,
    pub finish_reason: Option<String>,
    /// The model's text reply (may be empty if the turn was tool-call only).
    pub text: String,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    /// Same encoding as [`UserMessagePayload::channel_origin`] —
    /// the transport the agent's reply went out on (when bridged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_origin: Option<String>,
}

/// Configuration for one turn.
#[derive(Clone)]
pub struct TurnConfig {
    pub model: ModelId,
    pub system_prompt: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Hard cap on tool-call rounds within a single turn, to prevent
    /// runaway loops. Each model→tool→model→tool bounce counts as one.
    pub max_tool_rounds: u32,
    /// The tool set the model sees in the `tools` array.
    pub tools: Vec<ToolDeclaration>,
    /// HMAC key for event-log signing (§7.8). `None` during tests and
    /// pre-setup; production always sets this from the server's shared
    /// key so every row the executor writes is tamper-evident.
    pub event_log_hmac_key: Option<Vec<u8>>,
    /// Optional FSM-phase observer. The runner calls
    /// `observer.observe(Phase::AwaitingTool)` before tool dispatch
    /// and `observe(Phase::Thinking)` after the tool round finishes.
    /// Server wires this to `EventBus::publish(ConversationPhaseChanged)`.
    /// `None` during tests skips publishing — the runner's behaviour
    /// is identical either way.
    pub phase_observer: Option<Arc<dyn PhaseObserver>>,
    /// 2026-04-28 — forwarded as `chat_template_kwargs.enable_thinking`
    /// in the OpenAI-compatible POST body. Qwen3.5 reads this knob in
    /// its chat template; `false` suppresses the model's native
    /// `<think>` reasoning blocks. Mirror of the operator-editable
    /// `config_backends.reasoning_enabled` flag (defaults to false).
    pub reasoning_enabled: bool,
    /// Originating transport name when this turn was triggered by
    /// an inbound message from a bridged transport (signal / email /
    /// voice / sms). Threaded into the user_msg + model_turn
    /// payloads the executor commits so the SPA can render
    /// per-message channel icons. None for the default web path.
    pub inbound_channel_origin: Option<String>,
}

impl std::fmt::Debug for TurnConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnConfig")
            .field("model", &self.model)
            .field("system_prompt_len", &self.system_prompt.len())
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("max_tool_rounds", &self.max_tool_rounds)
            .field("tools_len", &self.tools.len())
            .field("hmac_key_set", &self.event_log_hmac_key.is_some())
            .field("phase_observer_set", &self.phase_observer.is_some())
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum TurnError {
    #[error("inference: {0}")]
    Inference(#[from] InferenceError),
    #[error("db: {0}")]
    Db(#[from] execlaw_core::db::DbError),
    #[error("turn exceeded max_tool_rounds ({0})")]
    MaxRounds(u32),
}

#[derive(Debug, Clone)]
pub struct TurnSummary {
    pub events_written: Vec<EventRecord>,
    pub assistant_text: String,
    pub tool_rounds: u32,
}

// ---------------------------------------------------------------------------
// Turn executor
// ---------------------------------------------------------------------------

/// Runs a single turn on behalf of a conversation. Stateless; safe to
/// construct per-turn.
pub struct TurnExecutor {
    pub inference: InferenceClient,
    pub tool_dispatch: Arc<dyn ToolDispatch>,
}

impl TurnExecutor {
    pub fn new(inference: InferenceClient, tool_dispatch: Arc<dyn ToolDispatch>) -> Self {
        Self {
            inference,
            tool_dispatch,
        }
    }

    /// Execute one turn:
    ///   1. Append a `user_msg` event to the log.
    ///   2. Assemble chat messages from the log.
    ///   3. Call the model; loop on tool_calls.
    ///   4. Commit everything via `EventLog::commit_turn`.
    pub async fn run_turn(
        &self,
        db: &Database,
        conversation_id: &ConversationId,
        user_text: &str,
        sender_principal_id: Option<String>,
        cfg: &TurnConfig,
    ) -> Result<TurnSummary, TurnError> {
        // Backward-compatible call site: no attachments. Internally
        // routes to `run_turn_with_attachments` with empty vecs.
        self.run_turn_with_attachments(
            db,
            conversation_id,
            user_text,
            sender_principal_id,
            cfg,
            Vec::new(),
            Vec::new(),
        )
        .await
    }

    /// Vision-aware turn driver. Same shape as `run_turn` but the
    /// caller can supply:
    ///   * `attachment_ids` — id references into `state_attachments`
    ///     stamped onto the `user_msg` event payload so subsequent
    ///     history replays know this turn carried images.
    ///   * `user_image_urls` — pre-encoded `data:<mime>;base64,...`
    ///     URLs the caller built from those attachments. When
    ///     non-empty, the trailing user message in the chat array
    ///     gets replaced with an OpenAI vision content array
    ///     (`ChatMessage::user_with_images`) so the inference
    ///     backend sees the images.
    ///
    /// The two are passed separately so the executor doesn't need
    /// access to `AttachmentStore` (it lives in execlaw-core, which
    /// runner-local can't depend on by design — runners run in a
    /// separate container with no DB).
    pub async fn run_turn_with_attachments(
        &self,
        db: &Database,
        conversation_id: &ConversationId,
        user_text: &str,
        sender_principal_id: Option<String>,
        cfg: &TurnConfig,
        attachment_ids: Vec<String>,
        user_image_urls: Vec<String>,
    ) -> Result<TurnSummary, TurnError> {
        // 1. Record the inbound user message as its own event so it's in
        //    the log before we ask the model anything. The log is keyed
        //    with the same HMAC as the server-side append path, so all
        //    rows written in this turn are tamper-evident.
        let log = match &cfg.event_log_hmac_key {
            Some(k) => EventLog::new(db).with_hmac_key(k.clone()),
            None => EventLog::new(db),
        };
        let user_seq = log.last_seq(conversation_id)?.next();
        let user_event = EventRecord::new(
            conversation_id.clone(),
            user_seq,
            EventKind::UserMsg,
            &UserMessagePayload {
                text: user_text.to_owned(),
                sender_principal_id: sender_principal_id.clone(),
                channel_origin: cfg.inbound_channel_origin.clone(),
                attachment_ids: attachment_ids.clone(),
            },
            sender_principal_id,
        )?;
        log.append(&user_event)?;

        // 2. Assemble the chat messages from the event log.
        let history = log.replay_since(conversation_id, EventSeq(0))?;
        let mut messages: Vec<ChatMessage> = vec![ChatMessage::system(&cfg.system_prompt)];
        messages.extend(hydrate_messages(&history));

        // 2026-05-15 — when the caller supplied image data URLs for
        // THIS turn, replace the trailing text-only user ChatMessage
        // with an OpenAI vision content array so the inference
        // backend sees the images. Mirrors the equivalent block in
        // `chats.rs::run_real_turn`. Prior turns' images are not
        // re-encoded here (the hydrate_messages path is text-only);
        // multi-turn vision is a known follow-up.
        if !user_image_urls.is_empty() {
            // Pull the previously-pushed text-only user message
            // (the current turn's content). Fall back to the raw
            // `user_text` if the history projection somehow elided
            // it (defensive — hydrate_messages always emits a
            // ChatMessage for the user_msg we just appended).
            let last_user_text = match messages.last() {
                Some(m) if matches!(m.role, Role::User) => {
                    let text = m.content.as_ref().map(|c| c.as_text()).unwrap_or_default();
                    messages.pop();
                    text
                }
                _ => user_text.to_owned(),
            };
            messages.push(ChatMessage::user_with_images(
                last_user_text,
                user_image_urls,
            ));
        }

        // 3. Tool-call loop.
        let mut pending: Vec<PendingEvent> = Vec::new();
        let mut tool_ordinal: u32 = 0;
        let mut rounds: u32 = 0;
        let mut last_text: String = String::new();
        let mut prompt_tokens: Option<u32> = None;
        let mut completion_tokens: Option<u32> = None;
        // 2026-05-12 — turn-timing instrumentation. Routed to the
        // dedicated `agent::turn_timing` target so it stays OFF by
        // default (enable with RUST_LOG=agent::turn_timing=debug)
        // and a future `info`-level dashboard widget can't drown
        // in per-round chatter. All measurements are wall-clock,
        // matched to the same monotonic clock — the deltas between
        // them are what's useful, not the absolute values.
        let turn_started_at = std::time::Instant::now();
        let conversation_id_str = conversation_id.as_str().to_owned();
        tracing::debug!(
            target: "agent::turn_timing",
            conversation_id = %conversation_id_str,
            tool_catalog_count = cfg.tools.len(),
            history_msg_count = messages.len(),
            "turn starting (in-process executor)"
        );

        loop {
            if rounds >= cfg.max_tool_rounds {
                // Record a cancelled model turn with a clear reason so the
                // transcript isn't a dangling prompt.
                pending.push(PendingEvent::encode(
                    EventKind::LlmCancelled,
                    &serde_json::json!({
                        "reason": "max_tool_rounds_exceeded",
                        "rounds": rounds,
                    }),
                    Some("system".into()),
                )?);
                return Err(TurnError::MaxRounds(cfg.max_tool_rounds));
            }

            let req = ChatRequest {
                model: cfg.model.clone(),
                messages: messages.clone(),
                tools: Some(cfg.tools.clone()),
                stream: false,
                temperature: cfg.temperature,
                max_tokens: cfg.max_tokens,
                // 2026-04-28 — forward the operator's reasoning toggle
                // into Qwen's chat template. Defaults to false on
                // every TurnConfig — see the field doc.
                chat_template_kwargs: Some(serde_json::json!({
                    "enable_thinking": cfg.reasoning_enabled,
                })),
            };
            // Per-round inference call. Time it so the operator can
            // tell the model spent N seconds generating vs. N seconds
            // on prefill (when usage is reported). vLLM's non-streaming
            // response arrives after generation completes so this
            // duration is the total round-trip including server-side
            // queue + prefill + decode.
            let inference_started_at = std::time::Instant::now();
            let inference_messages_count = messages.len();
            let inference_tools_count = cfg.tools.len();
            let resp = self.inference.chat_completions(&req).await?;
            let inference_elapsed_ms = inference_started_at.elapsed().as_millis() as u64;
            let choice = match resp.choices.first() {
                Some(c) => c.clone(),
                None => {
                    // Defensive: treat no-choices as a failed turn.
                    break;
                }
            };

            let finish_reason = choice.finish_reason.clone();
            if let Some(u) = &resp.usage {
                prompt_tokens = Some(u.prompt_tokens);
                completion_tokens = Some(u.completion_tokens);
            }
            // Per-round inference timing. The (prompt_tokens,
            // completion_tokens) pair lets the operator compute
            // prefill tps and decode tps after the fact; we don't
            // log those derived numbers because they're trivially
            // computed from the raw counts.
            tracing::debug!(
                target: "agent::turn_timing",
                conversation_id = %conversation_id_str,
                round = rounds,
                inference_ms = inference_elapsed_ms,
                request_messages = inference_messages_count,
                request_tools = inference_tools_count,
                prompt_tokens = resp.usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0),
                completion_tokens = resp.usage.as_ref().map(|u| u.completion_tokens).unwrap_or(0),
                finish_reason = ?finish_reason,
                tool_calls_returned = choice.message.tool_calls.len(),
                "round inference complete"
            );

            // Append the assistant message to our working transcript for
            // any subsequent rounds.
            let assistant_content = choice
                .message
                .content
                .as_ref()
                .map(|c| c.as_text())
                .unwrap_or_default();
            last_text = assistant_content.clone();
            messages.push(ChatMessage {
                role: Role::Assistant,
                content: choice.message.content.clone(),
                tool_call_id: None,
                name: None,
                tool_calls: choice.message.tool_calls.clone(),
            });

            if choice.message.tool_calls.is_empty() {
                // Terminal: record the model turn and exit the loop.
                pending.push(PendingEvent::encode(
                    EventKind::ModelTurn,
                    &ModelTurnPayload {
                        model: resp.model.clone(),
                        finish_reason,
                        text: assistant_content,
                        prompt_tokens,
                        completion_tokens,
                        channel_origin: cfg.inbound_channel_origin.clone(),
                    },
                    Some("agent".into()),
                )?);
                break;
            }

            // Phase 11.A — signal that the agent is now in a tool
            // round. Transports use this to keep the typing
            // indicator on through dispatch even though the LLM is
            // momentarily idle. We notify *once* per round
            // regardless of how many parallel tool calls the model
            // emitted; the is_processing classification on the
            // consumer side dedupes back-to-back transitions.
            if let Some(obs) = cfg.phase_observer.as_ref() {
                obs.observe(Phase::AwaitingTool);
            }

            // Dispatch each tool call, producing paired use/result events.
            // We also time each dispatch so the operator can tell
            // "model spent 4 minutes deciding what to call" from
            // "the tool itself took 4 minutes" (research_start vs
            // open_meteo.ensemble are wildly different latencies).
            let mut round_tool_dispatch_ms: u64 = 0;
            for tc in &choice.message.tool_calls {
                let args: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                    .unwrap_or_else(|_| serde_json::json!({}));

                pending.push(PendingEvent::encode(
                    EventKind::ToolUse,
                    &ToolUsePayload {
                        ordinal: tool_ordinal,
                        tool_name: tc.function.name.clone(),
                        args_json: args.clone(),
                    },
                    Some("agent".into()),
                )?);

                tracing::info!(
                    target: "executor::tool_dispatch",
                    round = rounds,
                    ordinal = tool_ordinal,
                    tool = %tc.function.name,
                    "agent dispatching tool",
                );
                let tool_started_at = std::time::Instant::now();
                let outcome = self.tool_dispatch.call(&tc.function.name, &args).await;
                let tool_elapsed_ms = tool_started_at.elapsed().as_millis() as u64;
                round_tool_dispatch_ms = round_tool_dispatch_ms.saturating_add(tool_elapsed_ms);
                tracing::debug!(
                    target: "agent::turn_timing",
                    conversation_id = %conversation_id_str,
                    round = rounds,
                    ordinal = tool_ordinal,
                    tool = %tc.function.name,
                    tool_ms = tool_elapsed_ms,
                    ok = outcome.is_ok(),
                    "tool dispatch complete"
                );
                match &outcome {
                    Ok(_) => tracing::info!(
                        target: "executor::tool_dispatch",
                        round = rounds,
                        ordinal = tool_ordinal,
                        tool = %tc.function.name,
                        "tool ok",
                    ),
                    Err(e) => tracing::warn!(
                        target: "executor::tool_dispatch",
                        round = rounds,
                        ordinal = tool_ordinal,
                        tool = %tc.function.name,
                        error = %e,
                        "tool failed",
                    ),
                }

                let result_payload = ToolResultPayload {
                    ordinal: tool_ordinal,
                    outcome: match outcome {
                        Ok(v) => Ok(v.clone()),
                        Err(e) => Err(e),
                    },
                };
                pending.push(PendingEvent::encode(
                    EventKind::ToolResult,
                    &result_payload,
                    Some("system".into()),
                )?);

                // Feed the tool result back into the chat history for the
                // next round.
                let feedback = serde_json::to_string(&result_payload.outcome)
                    .unwrap_or_else(|_| "{\"outcome\":\"encoding_failed\"}".into());
                messages.push(ChatMessage::tool_result(&tc.id, feedback));

                tool_ordinal += 1;
            }

            // Per-round summary covering both the inference call
            // (separately logged above) AND the aggregate tool
            // dispatch time, so a single line tells the whole
            // story of round N. The model_inference_ms /
            // tool_dispatch_ms split here mirrors the way
            // production agents are typically profiled (langfuse
            // / langsmith spans).
            tracing::debug!(
                target: "agent::turn_timing",
                conversation_id = %conversation_id_str,
                round = rounds,
                model_inference_ms = inference_elapsed_ms,
                tool_dispatch_ms = round_tool_dispatch_ms,
                tool_calls = choice.message.tool_calls.len(),
                "round complete (tool round)"
            );
            rounds += 1;

            // Tool round done; the agent is back to LLM-bound thinking.
            // Idle is *never* published from here — that's chats.rs's
            // job after the whole pipeline (including audit + broadcast)
            // finishes. is_processing() classifies both states as busy
            // so the indicator stays on without flicker.
            if let Some(obs) = cfg.phase_observer.as_ref() {
                obs.observe(Phase::Thinking);
            }
        }

        // 4. Commit the turn atomically. `commit_turn` enforces the
        //    tool_use/tool_result pairing invariant for us.
        let base_seq = log.last_seq(conversation_id)?;
        let written = log.commit_turn(conversation_id, base_seq, pending)?;

        // Kick the conversation row so UI observers see the new last_seq.
        // (Phase 1 could also update phase → idle here.)
        let store = ConversationStore::new(db);
        if let Some(mut row) = store.get(conversation_id)? {
            row.last_seq = log.last_seq(conversation_id)?;
            store.upsert(&row)?;
        }

        // Total turn timing. `total_ms` includes the user_msg
        // append + history hydrate + every round (inference +
        // tool dispatch) + final commit. Useful for the operator-
        // visible "why did this turn take N seconds" diagnosis:
        // subtract the per-round totals from `total_ms` to size
        // the host-side overhead.
        let total_ms = turn_started_at.elapsed().as_millis() as u64;
        tracing::debug!(
            target: "agent::turn_timing",
            conversation_id = %conversation_id_str,
            total_ms,
            tool_rounds = rounds,
            total_prompt_tokens = prompt_tokens.unwrap_or(0),
            total_completion_tokens = completion_tokens.unwrap_or(0),
            assistant_text_chars = last_text.chars().count(),
            "turn complete (in-process executor)"
        );
        Ok(TurnSummary {
            events_written: written,
            assistant_text: last_text,
            tool_rounds: rounds,
        })
    }
}

/// Convert a span of event-log records into chat messages for the next
/// model call. Phase 1 handles user_msg + model_turn + tool_use/tool_result
/// pairs; richer event kinds (voice, etc.) are skipped over for text turns.
fn hydrate_messages(events: &[EventRecord]) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::new();
    let mut pending_tool_calls: Vec<ToolCall> = Vec::new();

    for ev in events {
        match ev.kind {
            EventKind::UserMsg => {
                if let Ok(p) = ev.decode_payload::<UserMessagePayload>() {
                    out.push(ChatMessage::user(p.text));
                }
            }
            EventKind::ModelTurn => {
                if let Ok(p) = ev.decode_payload::<ModelTurnPayload>() {
                    let mut m = ChatMessage::assistant(p.text);
                    // If there were tool calls in the same turn, we attach
                    // them to the assistant message so the follow-up tool
                    // messages align.
                    std::mem::swap(&mut m.tool_calls, &mut pending_tool_calls);
                    out.push(m);
                }
            }
            EventKind::ToolUse => {
                if let Ok(p) = ev.decode_payload::<ToolUsePayload>() {
                    pending_tool_calls.push(ToolCall {
                        id: format!("call_{}", p.ordinal),
                        kind: "function".into(),
                        function: execlaw_inference_api::ToolCallFunction {
                            name: p.tool_name,
                            arguments: p.args_json.to_string(),
                        },
                    });
                }
            }
            EventKind::ToolResult => {
                if let Ok(p) = ev.decode_payload::<ToolResultPayload>() {
                    let body = match &p.outcome {
                        Ok(v) => v.to_string(),
                        Err(e) => serde_json::json!({"error": e}).to_string(),
                    };
                    out.push(ChatMessage::tool_result(
                        format!("call_{}", p.ordinal),
                        body,
                    ));
                }
            }
            _ => { /* other event kinds don't surface to the model */ }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::{Database, DbConfig};
    use execlaw_core::migrations::MigrationRunner;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    struct NullTools;

    #[async_trait]
    impl ToolDispatch for NullTools {
        async fn call(
            &self,
            _name: &str,
            _args: &serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Err("no tools wired".into())
        }
    }

    struct ChainedMockServer {
        responses: Vec<String>,
        served: AtomicUsize,
    }

    async fn run_mock_server(server: Arc<ChainedMockServer>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => break,
                };
                let idx = server.served.fetch_add(1, Ordering::SeqCst);
                let body = server
                    .responses
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| server.responses.last().cloned().unwrap_or_default());
                let mut buf = [0u8; 8192];
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(300),
                    sock.read(&mut buf),
                )
                .await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn simple_text_turn_commits_user_and_model_events() {
        let db = fresh_db();

        let canned = r#"{
            "id": "r1",
            "model": "Qwen3.5-27B-AWQ",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi there"},
                "finish_reason": "stop"
            }]
        }"#
        .to_owned();
        let server = Arc::new(ChainedMockServer {
            responses: vec![canned],
            served: AtomicUsize::new(0),
        });
        let addr = run_mock_server(server.clone()).await;

        let exec = TurnExecutor::new(
            InferenceClient::new(format!("http://{addr}/v1")),
            Arc::new(NullTools),
        );
        let cid = ConversationId::from("conv-simple");
        let cfg = TurnConfig {
            model: ModelId("QuantTrio/Qwen3.5-27B-AWQ".to_owned()),
            system_prompt: "test".into(),
            temperature: None,
            max_tokens: None,
            max_tool_rounds: 3,
            tools: vec![],
            event_log_hmac_key: None,
            phase_observer: None,
            reasoning_enabled: false,
            inbound_channel_origin: None,
        };

        let summary = exec
            .run_turn(&db, &cid, "hello", Some("pri-1".into()), &cfg)
            .await
            .unwrap();

        assert_eq!(summary.assistant_text, "hi there");
        assert_eq!(summary.tool_rounds, 0);

        let log = EventLog::new(&db);
        let events = log.replay_since(&cid, EventSeq(0)).unwrap();
        // user_msg + model_turn
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, EventKind::UserMsg);
        assert_eq!(events[1].kind, EventKind::ModelTurn);
    }

    #[tokio::test]
    async fn tool_call_turn_produces_paired_tool_events() {
        let db = fresh_db();

        // Response 1 asks to call a tool.
        let r1 = r#"{
            "id": "r1",
            "model": "Qwen3.5-27B-AWQ",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "tc1",
                        "type": "function",
                        "function": {"name": "echo", "arguments": "{\"msg\":\"ping\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }"#
        .to_owned();
        // Response 2 is the final text after the tool result is provided.
        let r2 = r#"{
            "id": "r2",
            "model": "Qwen3.5-27B-AWQ",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok got pong"},
                "finish_reason": "stop"
            }]
        }"#
        .to_owned();

        let server = Arc::new(ChainedMockServer {
            responses: vec![r1, r2],
            served: AtomicUsize::new(0),
        });
        let addr = run_mock_server(server.clone()).await;

        struct EchoTool;
        #[async_trait]
        impl ToolDispatch for EchoTool {
            async fn call(
                &self,
                _name: &str,
                args: &serde_json::Value,
            ) -> Result<serde_json::Value, String> {
                Ok(serde_json::json!({
                    "echoed": args.get("msg").cloned().unwrap_or(serde_json::Value::Null)
                }))
            }
        }

        let exec = TurnExecutor::new(
            InferenceClient::new(format!("http://{addr}/v1")),
            Arc::new(EchoTool),
        );
        let cid = ConversationId::from("conv-tool");
        let cfg = TurnConfig {
            model: ModelId("QuantTrio/Qwen3.5-27B-AWQ".to_owned()),
            system_prompt: "test".into(),
            temperature: None,
            max_tokens: None,
            max_tool_rounds: 3,
            tools: vec![ToolDeclaration::function(
                "echo",
                "echo the arg",
                serde_json::json!({"type":"object"}),
            )],
            event_log_hmac_key: None,
            phase_observer: None,
            reasoning_enabled: false,
            inbound_channel_origin: None,
        };

        let summary = exec
            .run_turn(&db, &cid, "do the thing", None, &cfg)
            .await
            .unwrap();

        assert_eq!(summary.tool_rounds, 1);
        assert_eq!(summary.assistant_text, "ok got pong");

        // Phase 11.A — repeat the same turn with a phase observer
        // attached and assert it sees the AwaitingTool→Thinking
        // transition for each round. We use a separate
        // conversation so the events from the prior turn don't
        // contaminate this assertion.
        struct Recorder {
            seen: std::sync::Mutex<Vec<Phase>>,
        }
        impl PhaseObserver for Recorder {
            fn observe(&self, phase: Phase) {
                self.seen.lock().unwrap().push(phase);
            }
        }
        let recorder = std::sync::Arc::new(Recorder {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let addr2 = run_mock_server(Arc::new(ChainedMockServer {
            responses: vec![
                r#"{"id":"r1","model":"m","choices":[{"index":0,
                    "message":{"role":"assistant","content":null,
                        "tool_calls":[{"id":"tc1","type":"function",
                            "function":{"name":"echo","arguments":"{}"}}]},
                    "finish_reason":"tool_calls"}]}"#
                    .to_owned(),
                r#"{"id":"r2","model":"m","choices":[{"index":0,
                    "message":{"role":"assistant","content":"done"},
                    "finish_reason":"stop"}]}"#
                    .to_owned(),
            ],
            served: AtomicUsize::new(0),
        }))
        .await;
        let exec2 = TurnExecutor::new(
            InferenceClient::new(format!("http://{addr2}/v1")),
            Arc::new(EchoTool),
        );
        let cid2 = ConversationId::from("conv-phase-obs");
        let cfg2 = TurnConfig {
            phase_observer: Some(recorder.clone() as std::sync::Arc<dyn PhaseObserver>),
            ..cfg.clone()
        };
        let _ = exec2.run_turn(&db, &cid2, "go", None, &cfg2).await.unwrap();
        let seen = recorder.seen.lock().unwrap().clone();
        // One round → AwaitingTool then Thinking.
        assert_eq!(
            seen,
            vec![Phase::AwaitingTool, Phase::Thinking],
            "observer must see exactly one tool round's transitions",
        );

        // Verify the event log: user_msg + tool_use + tool_result + model_turn
        let log = EventLog::new(&db);
        let events = log.replay_since(&cid, EventSeq(0)).unwrap();
        let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::UserMsg,
                EventKind::ToolUse,
                EventKind::ToolResult,
                EventKind::ModelTurn,
            ]
        );

        // The pairing invariant should hold (same ordinal for use+result).
        let use_ord: ToolUsePayload = events[1].decode_payload().unwrap();
        let res_ord: ToolResultPayload = events[2].decode_payload().unwrap();
        assert_eq!(use_ord.ordinal, res_ord.ordinal);
        assert!(res_ord.outcome.is_ok());
    }

    /// Adversarial: a tool handler that returns `Err` must still produce
    /// a paired `tool_result` event whose outcome is the Err message.
    /// This is the tool_use/tool_result pairing invariant under failure.
    #[tokio::test]
    async fn tool_dispatch_error_is_paired_as_err_outcome() {
        let db = fresh_db();

        let r1 = r#"{
            "id":"r1","model":"m","choices":[{
                "index":0,
                "message":{"role":"assistant","content":null,
                    "tool_calls":[{"id":"tc1","type":"function",
                        "function":{"name":"boom","arguments":"{}"}}]},
                "finish_reason":"tool_calls"
            }]
        }"#
        .to_owned();
        let r2 = r#"{
            "id":"r2","model":"m","choices":[{
                "index":0,
                "message":{"role":"assistant","content":"sorry failed"},
                "finish_reason":"stop"
            }]
        }"#
        .to_owned();
        let server = Arc::new(ChainedMockServer {
            responses: vec![r1, r2],
            served: AtomicUsize::new(0),
        });
        let addr = run_mock_server(server.clone()).await;

        struct FailingTool;
        #[async_trait]
        impl ToolDispatch for FailingTool {
            async fn call(
                &self,
                _name: &str,
                _args: &serde_json::Value,
            ) -> Result<serde_json::Value, String> {
                Err("planned failure".into())
            }
        }

        let exec = TurnExecutor::new(
            InferenceClient::new(format!("http://{addr}/v1")),
            Arc::new(FailingTool),
        );
        let cid = ConversationId::from("conv-tool-err");
        let cfg = TurnConfig {
            model: ModelId("m".to_owned()),
            system_prompt: "t".into(),
            temperature: None,
            max_tokens: None,
            max_tool_rounds: 3,
            tools: vec![ToolDeclaration::function(
                "boom",
                "always fails",
                serde_json::json!({"type":"object"}),
            )],
            event_log_hmac_key: None,
            phase_observer: None,
            reasoning_enabled: false,
            inbound_channel_origin: None,
        };
        let _ = exec
            .run_turn(&db, &cid, "try it", None, &cfg)
            .await
            .unwrap();

        let log = EventLog::new(&db);
        let events = log.replay_since(&cid, EventSeq(0)).unwrap();
        let result_ev = events
            .iter()
            .find(|e| e.kind == EventKind::ToolResult)
            .expect("must have a tool_result");
        let payload: ToolResultPayload = result_ev.decode_payload().unwrap();
        match &payload.outcome {
            Err(msg) => assert!(msg.contains("planned failure")),
            Ok(_) => panic!("expected Err outcome, got Ok"),
        }
    }

    /// Runaway-loop protection: if the model keeps emitting tool_calls
    /// past `max_tool_rounds`, `run_turn` must return `TurnError::MaxRounds`.
    #[tokio::test]
    async fn turn_errors_when_max_tool_rounds_exceeded() {
        let db = fresh_db();

        // Always return a tool-call response — the loop will never
        // reach a terminal assistant message.
        let looping = r#"{
            "id":"rN","model":"m","choices":[{
                "index":0,
                "message":{"role":"assistant","content":null,
                    "tool_calls":[{"id":"tcx","type":"function",
                        "function":{"name":"loop","arguments":"{}"}}]},
                "finish_reason":"tool_calls"
            }]
        }"#
        .to_owned();
        let server = Arc::new(ChainedMockServer {
            responses: vec![looping.clone(); 10],
            served: AtomicUsize::new(0),
        });
        let addr = run_mock_server(server.clone()).await;

        struct NoopTool;
        #[async_trait]
        impl ToolDispatch for NoopTool {
            async fn call(
                &self,
                _name: &str,
                _args: &serde_json::Value,
            ) -> Result<serde_json::Value, String> {
                Ok(serde_json::json!({}))
            }
        }

        let exec = TurnExecutor::new(
            InferenceClient::new(format!("http://{addr}/v1")),
            Arc::new(NoopTool),
        );
        let cid = ConversationId::from("conv-runaway");
        let cfg = TurnConfig {
            model: ModelId("m".into()),
            system_prompt: "t".into(),
            temperature: None,
            max_tokens: None,
            max_tool_rounds: 2, // hard cap
            tools: vec![ToolDeclaration::function(
                "loop",
                "infinite",
                serde_json::json!({"type":"object"}),
            )],
            event_log_hmac_key: None,
            phase_observer: None,
            reasoning_enabled: false,
            inbound_channel_origin: None,
        };
        let err = exec
            .run_turn(&db, &cid, "go", None, &cfg)
            .await
            .expect_err("should exceed max_tool_rounds");
        match err {
            TurnError::MaxRounds(n) => assert_eq!(n, 2),
            other => panic!("wrong error: {other:?}"),
        }
    }
}
