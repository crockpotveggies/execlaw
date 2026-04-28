//! Turn-execution loop for the runner.
//!
//! v1: streaming inference + token deltas + final commit. No tool
//! dispatch yet — the supervisor passes `tool_catalog: []` so the
//! model never emits tool_calls. Phase 4-follow-up adds RPC tool
//! dispatch and the workspace-tool family.

use crate::connect::{Connection, RunnerConfig};
use anyhow::Result;
use execlaw_inference_api::{ChatMessage, ChatRequest, InferenceClient, ModelId, Role};
use execlaw_runner_protocol::{RunnerToServer, ServerToRunner, ShutdownReason, TurnRequest};
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;

/// One cancel flag per in-flight `turn_id`. The main loop sets the
/// flag when it sees `CancelTurn`; the running turn polls between
/// SSE chunks.
#[derive(Default)]
pub struct CancelFlags {
    flags: HashMap<String, Arc<AtomicBool>>,
}

impl CancelFlags {
    fn arm(&mut self, turn_id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.flags.insert(turn_id.to_owned(), flag.clone());
        flag
    }

    fn cancel(&mut self, turn_id: &str) {
        if let Some(f) = self.flags.get(turn_id) {
            f.store(true, Ordering::SeqCst);
        }
    }

    fn drop_turn(&mut self, turn_id: &str) {
        self.flags.remove(turn_id);
    }
}

/// Returns false when the runner should exit (Shutdown received).
pub async fn handle_frame(
    cfg: &RunnerConfig,
    conn: &mut Connection,
    cancel_flags: Arc<Mutex<CancelFlags>>,
    frame: ServerToRunner,
) -> bool {
    match frame {
        ServerToRunner::Heartbeat { nonce } => {
            let _ = conn
                .send(&RunnerToServer::HeartbeatAck { nonce })
                .await;
            true
        }
        ServerToRunner::Shutdown { reason } => {
            tracing::info!(?reason, "received shutdown");
            true_and_exit(reason)
        }
        ServerToRunner::CancelTurn { turn_id } => {
            cancel_flags.lock().await.cancel(&turn_id);
            tracing::info!(turn_id = %turn_id, "cancel flag armed");
            true
        }
        ServerToRunner::ToolCallResult(_result) => {
            // v1: no tool dispatch yet. Drop with a warning so a
            // future supervisor mismatch doesn't silently fail.
            tracing::warn!("tool call result received but tool dispatch not implemented");
            true
        }
        ServerToRunner::Turn(req) => {
            let flag = cancel_flags.lock().await.arm(&req.turn_id);
            let turn_id = req.turn_id.clone();
            // We run the turn inline (one turn at a time per
            // runner). If concurrency-per-runner becomes a goal
            // later, spawn here and key the cancel flags more
            // carefully. For now, sequential matches selfhosted-
            // claw's mental model and keeps memory predictable.
            if let Err(e) = run_turn(cfg, conn, flag.clone(), req).await {
                tracing::error!(error = %e, turn_id = %turn_id, "turn failed");
                let _ = conn
                    .send(&RunnerToServer::Error {
                        turn_id: turn_id.clone(),
                        conversation_id: String::new(),
                        message: format!("{e}"),
                        cancelled: false,
                    })
                    .await;
            }
            cancel_flags.lock().await.drop_turn(&turn_id);
            true
        }
    }
}

fn true_and_exit(reason: ShutdownReason) -> bool {
    // Returning false from handle_frame is what tells main() to
    // break the loop and exit. The reason is already logged.
    let _ = reason;
    false
}

async fn run_turn(
    _cfg: &RunnerConfig,
    conn: &mut Connection,
    cancel: Arc<AtomicBool>,
    req: TurnRequest,
) -> Result<()> {
    let client = InferenceClient::new(req.inference_url.clone());

    // Compose chat messages: system prompt + history + new user
    // text. (The supervisor passes the spotlight delimiter in
    // `req.spotlight`; we apply it here so the runner doesn't
    // expose un-wrapped untrusted content to the model.)
    let mut messages: Vec<ChatMessage> = Vec::with_capacity(req.history.len() + 2);
    messages.push(ChatMessage::system(&req.system_prompt));
    for m in req.history {
        messages.push(m);
    }
    let user_text = match req.spotlight {
        Some(delim) => format!("{delim}\n{}\n{delim}", req.user_text),
        None => req.user_text.clone(),
    };
    messages.push(ChatMessage {
        role: Role::User,
        content: Some(user_text),
        tool_call_id: None,
        name: None,
        tool_calls: vec![],
    });

    // Notify "thinking" so the SPA's typing indicator lights up.
    conn.send(&RunnerToServer::Phase {
        turn_id: req.turn_id.clone(),
        conversation_id: req.conversation_id.clone(),
        phase: "thinking".into(),
    })
    .await?;

    let chat_req = ChatRequest {
        model: ModelId(req.model.clone()),
        messages,
        tools: None,
        stream: true,
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        chat_template_kwargs: Some(serde_json::json!({
            "enable_thinking": req.reasoning_enabled,
        })),
    };
    let mut stream = client.chat_completions_stream(&chat_req).await?;

    let mut assembled = String::new();
    let mut model_id = req.model.clone();
    let mut finish_reason: Option<String> = None;
    // Streaming chunks don't carry usage (vLLM / OpenAI streaming
    // omits it). Token accounting will land later when we add a
    // server-side estimator or post-turn /usage call.
    let prompt_tokens: Option<u32> = None;
    let completion_tokens: Option<u32> = None;
    let mut was_cancelled = false;

    while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::SeqCst) {
            was_cancelled = true;
            break;
        }
        let chunk = chunk?;
        if !chunk.model.is_empty() {
            model_id = chunk.model.clone();
        }
        for ch in &chunk.choices {
            if let Some(t) = &ch.delta.content {
                if !t.is_empty() {
                    assembled.push_str(t);
                    conn.send(&RunnerToServer::TokenDelta {
                        turn_id: req.turn_id.clone(),
                        conversation_id: req.conversation_id.clone(),
                        text: t.clone(),
                    })
                    .await?;
                }
            }
            if let Some(fr) = &ch.finish_reason {
                finish_reason = Some(fr.clone());
            }
        }
    }
    drop(stream);

    if was_cancelled {
        conn.send(&RunnerToServer::Error {
            turn_id: req.turn_id.clone(),
            conversation_id: req.conversation_id.clone(),
            message: "cancelled".into(),
            cancelled: true,
        })
        .await?;
        return Ok(());
    }

    let assistant_text = if assembled.is_empty() {
        "(empty response)".to_owned()
    } else {
        assembled
    };

    // Have the supervisor commit the model_turn event for us.
    let model_turn_payload = serde_json::json!({
        "model": model_id,
        "text": assistant_text.clone(),
        "finish_reason": finish_reason.clone(),
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
    });
    conn.send(&RunnerToServer::EventLogAppend {
        turn_id: req.turn_id.clone(),
        conversation_id: req.conversation_id.clone(),
        kind: "model_turn".into(),
        payload: model_turn_payload,
        actor: Some("agent".into()),
    })
    .await?;

    // Phase → idle is implied by `TurnComplete`, but the supervisor
    // currently broadcasts it as a separate event for the SPA's
    // existing typing-indicator wiring.
    conn.send(&RunnerToServer::Phase {
        turn_id: req.turn_id.clone(),
        conversation_id: req.conversation_id.clone(),
        phase: "idle".into(),
    })
    .await?;

    conn.send(&RunnerToServer::TurnComplete {
        turn_id: req.turn_id.clone(),
        conversation_id: req.conversation_id.clone(),
        assistant_text,
        finish_reason,
        prompt_tokens,
        completion_tokens,
    })
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_flags_arm_and_cancel() {
        let mut flags = CancelFlags::default();
        let f1 = flags.arm("t-1");
        assert!(!f1.load(Ordering::SeqCst));
        flags.cancel("t-1");
        assert!(f1.load(Ordering::SeqCst));
    }

    #[test]
    fn cancel_unknown_turn_is_noop() {
        let mut flags = CancelFlags::default();
        flags.cancel("nonexistent");
    }

    #[test]
    fn drop_turn_removes_flag() {
        let mut flags = CancelFlags::default();
        let _f = flags.arm("t-1");
        flags.drop_turn("t-1");
        // Re-arming the same id should give a fresh flag.
        let f2 = flags.arm("t-1");
        assert!(!f2.load(Ordering::SeqCst));
    }
}
