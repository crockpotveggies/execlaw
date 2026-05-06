//! Inference-backed implementation of [`execlaw_core::tool::SubagentApi`].
//!
//! Lives in `server` (not `core`) because it depends on
//! `execlaw_inference_api::InferenceClient` to make the child LLM
//! call. The dispatcher constructs one per turn via
//! `ChainedToolDispatch::with_inference(...)` so subagent calls
//! reach whichever Standard backend the operator's resolver
//! returned (managed-vLLM, OpenArc, etc.) — no fixed URL.
//!
//! Synchronous: the parent's turn pauses for the duration. For
//! multi-minute work that the operator should be able to inspect
//! mid-flight, use a research/job-system path instead.
//!
//! 2026-04-29.

use async_trait::async_trait;
use execlaw_core::Database;
use execlaw_core::events::{EventKind, EventLog, PendingEvent};
use execlaw_core::ids::ConversationId;
use execlaw_core::tool::{ApiError, SubagentApi, SubagentRequest, SubagentResponse};
use execlaw_inference_api::{ChatMessage, ChatRequest, InferenceClient, ModelId};
use serde::Serialize;
use std::sync::Arc;
use uuid::Uuid;

const DEFAULT_MAX_TOKENS: u32 = 1024;
const HARD_CAP_TOKENS: u32 = 4096;

/// System prompt the parent hands to the child. Deliberately
/// minimal: the parent's `task` + `context` is the entire substance
/// of the call.
const SUBAGENT_SYSTEM_PROMPT: &str = "You are a helper subagent invoked by another agent for a focused sub-task. \
Reply with exactly the requested output and nothing else — no preface, no \
disclaimer, no \"as an AI...\" framing. Keep replies tight and on-topic.";

#[derive(Debug, Serialize)]
struct SubagentStartedPayload {
    task_id: String,
    /// Truncated form of `task` so the typing-indicator pill has
    /// something compact to show without exposing 4 KB of prompt.
    task_preview: String,
}

#[derive(Debug, Serialize)]
struct SubagentCompletedPayload {
    task_id: String,
    tokens_used: Option<u32>,
    /// Whether the subagent succeeded; the SPA's pill flips back
    /// to the parent's typing state regardless, but operators
    /// auditing the log appreciate the explicit signal.
    succeeded: bool,
}

/// Inference-backed `SubagentApi`. Holds a cheap-clone
/// `Arc<InferenceClient>` + the model id; the dispatcher constructs
/// one per turn from the same resolver `chats.rs` already uses.
///
/// Each `delegate` call:
///   1. Mints a `task_id`, writes a `SubagentStarted` event
///   2. Makes a non-streaming chat-completion call (system + user)
///   3. Writes `SubagentCompleted` with token usage + status
///   4. Returns the text
///
/// The events flow through the existing WS event bus so the SPA's
/// typing-indicator pill subscribes once and renders the structured
/// detail line for any subagent any tool fires.
pub struct InferenceSubagentApi {
    client: Arc<InferenceClient>,
    model: String,
    db: Database,
    conversation_id: ConversationId,
}

impl InferenceSubagentApi {
    pub fn new(
        client: Arc<InferenceClient>,
        model: impl Into<String>,
        db: Database,
        conversation_id: ConversationId,
    ) -> Self {
        Self {
            client,
            model: model.into(),
            db,
            conversation_id,
        }
    }

    fn truncate_for_preview(s: &str) -> String {
        const MAX: usize = 80;
        let trimmed = s.trim();
        if trimmed.chars().count() <= MAX {
            return trimmed.to_owned();
        }
        let mut buf: String = trimmed.chars().take(MAX - 1).collect();
        buf.push('…');
        buf
    }

    fn emit_event<P: Serialize>(&self, kind: EventKind, payload: &P) -> Result<(), ApiError> {
        let log = EventLog::new(&self.db);
        let base = log
            .last_seq(&self.conversation_id)
            .map_err(|e| ApiError::Storage(format!("last_seq: {e}")))?;
        let bytes =
            rmp_serde::to_vec(payload).map_err(|e| ApiError::Storage(format!("encode: {e}")))?;
        let pending = PendingEvent {
            kind,
            payload: bytes,
            actor: Some("system".into()),
        };
        log.commit_turn(&self.conversation_id, base, vec![pending])
            .map_err(|e| ApiError::Storage(format!("commit: {e}")))?;
        Ok(())
    }
}

#[async_trait]
impl SubagentApi for InferenceSubagentApi {
    async fn delegate(&self, req: &SubagentRequest) -> Result<SubagentResponse, ApiError> {
        if req.task.trim().is_empty() {
            return Err(ApiError::Validation("task is empty".into()));
        }
        let task_id = Uuid::new_v4().to_string();
        let task_preview = Self::truncate_for_preview(&req.task);

        // Best-effort emit — DB hiccups shouldn't kill the subagent
        // call. The inference path is the load-bearing part.
        if let Err(e) = self.emit_event(
            EventKind::SubagentStarted,
            &SubagentStartedPayload {
                task_id: task_id.clone(),
                task_preview: task_preview.clone(),
            },
        ) {
            tracing::warn!(?e, "subagent started-event emit failed; continuing");
        }

        // Build the child's prompt: system + (optional) context +
        // task. The child sees no parent history; that's the
        // point — context isolation is the whole reason for using
        // a subagent.
        let mut messages: Vec<ChatMessage> = vec![ChatMessage::system(SUBAGENT_SYSTEM_PROMPT)];
        if let Some(ctx) = req.context.as_ref().filter(|s| !s.trim().is_empty()) {
            messages.push(ChatMessage::user(format!("Context:\n{ctx}")));
        }
        messages.push(ChatMessage::user(req.task.clone()));

        let max_tokens = req
            .max_tokens
            .unwrap_or(DEFAULT_MAX_TOKENS)
            .min(HARD_CAP_TOKENS);
        let chat_req = ChatRequest {
            model: ModelId(self.model.clone()),
            messages,
            max_tokens: Some(max_tokens),
            temperature: None,
            stream: false,
            tools: None,
            chat_template_kwargs: None,
        };

        let adapter = execlaw_model_adapter::adapter_for(
            execlaw_model_adapter::ModelFamily::detect(&self.model),
        );
        // Subagent reply is consumed verbatim by the parent turn;
        // markdown hint = no fence stripping (parent may want code
        // blocks if the subagent emitted them).
        let outcome = adapter
            .chat(
                &self.client,
                chat_req,
                execlaw_model_adapter::OutputHint::Markdown,
            )
            .await;
        let (text, tokens_used, succeeded) = match outcome {
            Ok(adapted) => {
                let tokens = adapted.usage.as_ref().map(|u| u.completion_tokens);
                (adapted.content, tokens, true)
            }
            Err(e) => {
                let _ = self.emit_event(
                    EventKind::SubagentCompleted,
                    &SubagentCompletedPayload {
                        task_id: task_id.clone(),
                        tokens_used: None,
                        succeeded: false,
                    },
                );
                return Err(ApiError::Storage(format!("inference: {e}")));
            }
        };

        if let Err(e) = self.emit_event(
            EventKind::SubagentCompleted,
            &SubagentCompletedPayload {
                task_id: task_id.clone(),
                tokens_used,
                succeeded,
            },
        ) {
            tracing::warn!(?e, "subagent completed-event emit failed; continuing");
        }

        Ok(SubagentResponse {
            text,
            task_id,
            tokens_used,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::conversation::{
        ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
    };
    use execlaw_core::db::DbConfig;
    use execlaw_core::ids::EventSeq;
    use execlaw_core::migrations::MigrationRunner;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn seed_conv(db: &Database, id: &str) -> ConversationId {
        let cid = ConversationId::from(id);
        ConversationStore::new(db)
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
                display_name_source: "auto".into(),
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: None,
                last_activity_at: 0,
            })
            .unwrap();
        cid
    }

    /// End-to-end test: spin up an in-process HTTP mock that
    /// returns a canned chat-completion response, point an
    /// InferenceClient at it, fire `delegate`, verify the response
    /// text comes through and both `SubagentStarted` +
    /// `SubagentCompleted` events landed in the log.
    #[tokio::test]
    async fn delegate_round_trips_against_mock_inference_backend() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let body = serde_json::json!({
                "id": "cmpl-test",
                "object": "chat.completion",
                "created": 1_700_000_000,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6},
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });

        let client = Arc::new(InferenceClient::new(format!("http://{addr}/v1")));
        let db = fresh_db();
        let cid = seed_conv(&db, "c1");
        let api = InferenceSubagentApi::new(client, "test-model", db.clone(), cid.clone());

        let resp = api
            .delegate(&SubagentRequest {
                task: "say ok".into(),
                context: None,
                max_tokens: Some(64),
            })
            .await
            .unwrap();

        assert_eq!(resp.text, "ok");
        assert!(resp.tokens_used.is_some());
        assert!(!resp.task_id.is_empty());

        // Verify both lifecycle events committed.
        let events = EventLog::new(&db).replay_since(&cid, EventSeq(0)).unwrap();
        let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EventKind::SubagentStarted));
        assert!(kinds.contains(&EventKind::SubagentCompleted));
    }

    #[tokio::test]
    async fn delegate_rejects_empty_task() {
        let client = Arc::new(InferenceClient::new("http://127.0.0.1:0/v1"));
        let db = fresh_db();
        let cid = seed_conv(&db, "c1");
        let api = InferenceSubagentApi::new(client, "m", db, cid);
        let err = api
            .delegate(&SubagentRequest {
                task: "   ".into(),
                context: None,
                max_tokens: None,
            })
            .await
            .unwrap_err();
        match err {
            ApiError::Validation(msg) => assert!(msg.contains("empty")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn task_preview_truncates_long_prompts() {
        let long = "a".repeat(200);
        let preview = InferenceSubagentApi::truncate_for_preview(&long);
        assert!(preview.chars().count() <= 80);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn task_preview_passes_through_short_prompts_intact() {
        let short = "draft the email";
        assert_eq!(
            InferenceSubagentApi::truncate_for_preview(short),
            "draft the email"
        );
    }
}
