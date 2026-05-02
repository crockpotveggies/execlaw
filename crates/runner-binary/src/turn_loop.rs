//! Turn-execution loop for the runner.
//!
//! Each `Turn(req)` frame from the supervisor spawns a tokio task
//! that owns a per-turn cancel flag + a per-turn `ToolCallResult`
//! mailbox. The task streams from vLLM, accumulates `tool_call`
//! deltas if the `tool_catalog` is non-empty, and on
//! `finish_reason = "tool_calls"` forwards each call to the
//! supervisor as a `RunnerToServer::ToolCallRequest`, parks on its
//! mailbox until the matching `ToolCallResult` arrives, then
//! re-issues the chat with the assistant + tool messages appended.
//!
//! The model runs the loop until it produces a regular `stop`
//! finish (or `length`, or any non-`tool_calls` reason). The
//! per-turn mailbox is keyed by `call_id` on the supervisor side,
//! so out-of-order replies still match. The `MAX_TOOL_ROUNDS`
//! bound is enforced server-side by the supervisor's policy
//! engine; the runner itself trusts the catalog it was handed.

use crate::connect::ConnectionTx;
use anyhow::{Context, Result, anyhow};
use execlaw_inference_api::{
    ChatMessage, ChatRequest, ChatStreamChoice, InferenceClient, ModelId, Role,
    ToolCall, ToolCallFunction, ToolCallDelta,
};
use execlaw_runner_protocol::{
    RunnerToServer, ToolCallResult, ToolOutcome, TurnRequest,
};
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;
use tokio::sync::Mutex;

/// One cancel flag per in-flight `turn_id`. Main loop sets the flag
/// when it sees `CancelTurn`; the running turn task polls between
/// SSE chunks and at every tool-loop boundary.
#[derive(Default)]
pub struct CancelFlags {
    flags: HashMap<String, Arc<AtomicBool>>,
}

impl CancelFlags {
    pub fn arm(&mut self, turn_id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.flags.insert(turn_id.to_owned(), flag.clone());
        flag
    }

    pub fn cancel(&mut self, turn_id: &str) {
        if let Some(f) = self.flags.get(turn_id) {
            f.store(true, Ordering::SeqCst);
        }
    }

    pub fn drop_turn(&mut self, turn_id: &str) {
        self.flags.remove(turn_id);
    }
}

/// Per-turn `ToolCallResult` mailbox. The main loop registers a
/// sender at turn-spawn time; the turn task owns the receiver. When
/// a `ServerToRunner::ToolCallResult` lands, the main loop looks
/// up `turn_id` here and forwards.
pub type ToolResultRoutes = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<ToolCallResult>>>>;

/// Hard upper bound on tool-call rounds before the runner gives up.
/// The supervisor's policy engine enforces a tighter, configurable
/// bound (`config.max_tool_rounds`); this is the runner's
/// belt-and-suspenders cap so a misconfigured server can't pin the
/// runner in an infinite loop.
pub const RUNNER_MAX_TOOL_ROUNDS: u32 = 16;

pub async fn run_turn(
    tx: ConnectionTx,
    cancel: Arc<AtomicBool>,
    mut tool_result_rx: mpsc::UnboundedReceiver<ToolCallResult>,
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
    let user_text = match &req.spotlight {
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
    tx.send(RunnerToServer::Phase {
        turn_id: req.turn_id.clone(),
        conversation_id: req.conversation_id.clone(),
        phase: "thinking".into(),
    })?;

    let tools = if req.tool_catalog.is_empty() {
        None
    } else {
        Some(req.tool_catalog.clone())
    };

    let mut model_id = req.model.clone();
    let final_assistant_text: String;
    let final_finish_reason: Option<String>;
    // Streaming chunks don't carry usage (vLLM / OpenAI streaming
    // omits it). Token accounting will land later when we add a
    // server-side estimator or post-turn /usage call.
    let prompt_tokens: Option<u32> = None;
    let completion_tokens: Option<u32> = None;
    let mut was_cancelled = false;
    let mut round: u32 = 0;

    loop {
        if round >= RUNNER_MAX_TOOL_ROUNDS {
            return Err(anyhow!(
                "runner hit RUNNER_MAX_TOOL_ROUNDS={RUNNER_MAX_TOOL_ROUNDS}; aborting turn"
            ));
        }
        round += 1;

        let chat_req = ChatRequest {
            model: ModelId(req.model.clone()),
            messages: messages.clone(),
            tools: tools.clone(),
            stream: true,
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            chat_template_kwargs: Some(serde_json::json!({
                "enable_thinking": req.reasoning_enabled,
            })),
        };
        let mut stream = client
            .chat_completions_stream(&chat_req)
            .await
            .context("opening inference stream")?;

        let mut text_acc = String::new();
        let mut tool_calls: Vec<ToolCallAcc> = Vec::new();
        let mut finish_reason: Option<String> = None;

        while let Some(chunk) = stream.next().await {
            if cancel.load(Ordering::SeqCst) {
                was_cancelled = true;
                break;
            }
            let chunk = chunk.context("reading inference stream chunk")?;
            if !chunk.model.is_empty() {
                model_id = chunk.model.clone();
            }
            for ch in &chunk.choices {
                accumulate_choice(
                    &tx,
                    &req.turn_id,
                    &req.conversation_id,
                    ch,
                    &mut text_acc,
                    &mut tool_calls,
                )?;
                if let Some(fr) = &ch.finish_reason {
                    finish_reason = Some(fr.clone());
                }
            }
        }
        drop(stream);

        if was_cancelled {
            tx.send(RunnerToServer::Error {
                turn_id: req.turn_id.clone(),
                conversation_id: req.conversation_id.clone(),
                message: "cancelled".into(),
                cancelled: true,
            })?;
            return Ok(());
        }

        let finish = finish_reason.clone().unwrap_or_default();

        // Defensive: vLLM occasionally returns finish_reason="tool_calls"
        // with `tool_calls: []` when the chosen `--tool-call-parser`
        // doesn't match the model's emitted format (e.g. running
        // Qwen3-XML output through the hermes parser). Pre-fix the
        // runner fell through to "non-tool finish" with empty
        // text_acc and committed "(empty response)" — invisible to
        // the operator beyond the SPA placeholder. Log loudly so the
        // root cause (parser mismatch, prompt-template breakage) is
        // findable in the runner journal.
        if finish == "tool_calls" && tool_calls.is_empty() {
            tracing::warn!(
                turn_id = %req.turn_id,
                conversation_id = %req.conversation_id,
                text_acc_len = text_acc.len(),
                text_acc_preview = %text_acc.chars().take(200).collect::<String>(),
                "model emitted finish_reason=tool_calls with zero parsed calls — \
                 the vLLM tool-call parser likely doesn't match the model's tool-call format. \
                 Check `--tool-call-parser` against the model's native output (Qwen3 → qwen3_xml, \
                 Qwen2.5 → hermes, Llama-3 → llama3_json)."
            );
        }

        if finish == "tool_calls" && !tool_calls.is_empty() {
            // Append the assistant's tool_calls turn to the
            // history. content stays Some(text_acc) so the model
            // remembers any reasoning it emitted alongside the
            // call. tool_calls carries the structured calls the
            // model produced.
            let assistant_calls: Vec<ToolCall> = tool_calls
                .iter()
                .map(ToolCallAcc::finalize)
                .collect();
            messages.push(ChatMessage {
                role: Role::Assistant,
                content: if text_acc.is_empty() {
                    None
                } else {
                    Some(text_acc.clone())
                },
                tool_call_id: None,
                name: None,
                tool_calls: assistant_calls.clone(),
            });

            // Forward each call to the supervisor and park on the
            // per-turn mailbox until the matching result arrives.
            // Out-of-order results are tolerated: we drain by
            // `call_id` until every outstanding call has resolved.
            let mut outstanding: HashMap<String, ToolCall> = HashMap::new();
            for call in &assistant_calls {
                outstanding.insert(call.id.clone(), call.clone());
                let parsed_args: serde_json::Value =
                    match serde_json::from_str(&call.function.arguments) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(
                                tool = %call.function.name,
                                error = %e,
                                "tool call arguments not valid JSON; sending raw"
                            );
                            serde_json::Value::String(call.function.arguments.clone())
                        }
                    };
                tx.send(RunnerToServer::ToolCallRequest {
                    turn_id: req.turn_id.clone(),
                    conversation_id: req.conversation_id.clone(),
                    call_id: call.id.clone(),
                    tool_name: call.function.name.clone(),
                    args: parsed_args,
                })?;
            }

            // Drain results. A `cancel` mid-tool-call still needs
            // to honour the outstanding result frames so we don't
            // dangle a half-finished round; we set the flag, send
            // the cancellation Error frame, and exit.
            while !outstanding.is_empty() {
                tokio::select! {
                    biased;
                    maybe_result = tool_result_rx.recv() => {
                        let Some(result) = maybe_result else {
                            return Err(anyhow!(
                                "tool result mailbox closed while {} call(s) outstanding",
                                outstanding.len()
                            ));
                        };
                        if outstanding.remove(&result.call_id).is_none() {
                            tracing::warn!(
                                call_id = %result.call_id,
                                "received tool result for unknown call_id; ignoring"
                            );
                            continue;
                        }
                        let content = match result.outcome {
                            ToolOutcome::Ok { value } => serde_json::to_string(&value)
                                .unwrap_or_else(|_| "\"<unrepresentable result>\"".into()),
                            ToolOutcome::Err { message } => serde_json::to_string(
                                &serde_json::json!({"error": message}),
                            )
                            .unwrap_or_else(|_| "{\"error\":\"<unrepresentable\"}".into()),
                        };
                        messages.push(ChatMessage::tool_result(
                            result.call_id,
                            content,
                        ));
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)), if cancel.load(Ordering::SeqCst) => {
                        was_cancelled = true;
                        break;
                    }
                }
            }
            if was_cancelled {
                tx.send(RunnerToServer::Error {
                    turn_id: req.turn_id.clone(),
                    conversation_id: req.conversation_id.clone(),
                    message: "cancelled".into(),
                    cancelled: true,
                })?;
                return Ok(());
            }
            // Loop back: ask the model for its next move with the
            // tool results in scope.
            continue;
        }

        // Non-tool finish — we're done.
        final_assistant_text = if text_acc.is_empty() {
            "(empty response)".to_owned()
        } else {
            text_acc
        };
        final_finish_reason = finish_reason;
        break;
    }

    // Have the supervisor commit the model_turn event for us.
    let model_turn_payload = serde_json::json!({
        "model": model_id,
        "text": final_assistant_text.clone(),
        "finish_reason": final_finish_reason.clone(),
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
    });
    tx.send(RunnerToServer::EventLogAppend {
        turn_id: req.turn_id.clone(),
        conversation_id: req.conversation_id.clone(),
        kind: "model_turn".into(),
        payload: model_turn_payload,
        actor: Some("agent".into()),
    })?;

    // Phase → idle is implied by `TurnComplete`, but the supervisor
    // currently broadcasts it as a separate event for the SPA's
    // existing typing-indicator wiring.
    tx.send(RunnerToServer::Phase {
        turn_id: req.turn_id.clone(),
        conversation_id: req.conversation_id.clone(),
        phase: "idle".into(),
    })?;

    tx.send(RunnerToServer::TurnComplete {
        turn_id: req.turn_id.clone(),
        conversation_id: req.conversation_id.clone(),
        assistant_text: final_assistant_text,
        finish_reason: final_finish_reason,
        prompt_tokens,
        completion_tokens,
    })?;

    Ok(())
}

/// Per-call accumulator for stream-segmented tool_calls. The OpenAI
/// streaming spec sends the `id` + `type` + `function.name` in the
/// first delta and accumulates `function.arguments` in subsequent
/// deltas — we have to stitch them back together before forwarding.
struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
    /// True once we've seen at least one delta with this index
    /// carrying an `id`. Useful for catching malformed streams
    /// (OpenAI requires the first chunk of a call to include id).
    has_id: bool,
}

impl ToolCallAcc {
    fn finalize(&self) -> ToolCall {
        ToolCall {
            id: self.id.clone(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: self.name.clone(),
                arguments: self.arguments.clone(),
            },
        }
    }
}

fn accumulate_choice(
    tx: &ConnectionTx,
    turn_id: &str,
    conversation_id: &str,
    choice: &ChatStreamChoice,
    text_acc: &mut String,
    tool_calls: &mut Vec<ToolCallAcc>,
) -> Result<()> {
    if let Some(t) = &choice.delta.content {
        if !t.is_empty() {
            text_acc.push_str(t);
            tx.send(RunnerToServer::TokenDelta {
                turn_id: turn_id.to_owned(),
                conversation_id: conversation_id.to_owned(),
                text: t.clone(),
            })?;
        }
    }
    for tc_delta in &choice.delta.tool_calls {
        accumulate_tool_call(tc_delta, tool_calls);
    }
    Ok(())
}

fn accumulate_tool_call(delta: &ToolCallDelta, acc: &mut Vec<ToolCallAcc>) {
    let idx = delta.index as usize;
    while acc.len() <= idx {
        acc.push(ToolCallAcc {
            id: String::new(),
            name: String::new(),
            arguments: String::new(),
            has_id: false,
        });
    }
    let entry = &mut acc[idx];
    if let Some(id) = &delta.id {
        if !id.is_empty() {
            entry.id = id.clone();
            entry.has_id = true;
        }
    }
    if let Some(func) = &delta.function {
        if let Some(n) = &func.name {
            if !n.is_empty() {
                entry.name = n.clone();
            }
        }
        if let Some(a) = &func.arguments {
            entry.arguments.push_str(a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_inference_api::ToolCallFunctionDelta;

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

    /// The OpenAI streaming spec sends the call header in one chunk
    /// and the arguments piece by piece in subsequent chunks. Our
    /// accumulator must reassemble these into one full call.
    #[test]
    fn tool_call_accumulator_stitches_streamed_deltas() {
        let mut acc: Vec<ToolCallAcc> = Vec::new();
        accumulate_tool_call(
            &ToolCallDelta {
                index: 0,
                id: Some("call_001".into()),
                kind: Some("function".into()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("add".into()),
                    arguments: Some("{\"a\":".into()),
                }),
            },
            &mut acc,
        );
        accumulate_tool_call(
            &ToolCallDelta {
                index: 0,
                id: None,
                kind: None,
                function: Some(ToolCallFunctionDelta {
                    name: None,
                    arguments: Some("1,\"b\":2}".into()),
                }),
            },
            &mut acc,
        );
        assert_eq!(acc.len(), 1);
        let call = acc[0].finalize();
        assert_eq!(call.id, "call_001");
        assert_eq!(call.function.name, "add");
        assert_eq!(call.function.arguments, "{\"a\":1,\"b\":2}");
    }

    /// Two concurrent calls in the same response (model dispatching
    /// a fan-out) must end up as two distinct accumulators, indexed
    /// by the delta's `index` field.
    #[test]
    fn tool_call_accumulator_handles_two_concurrent_calls() {
        let mut acc: Vec<ToolCallAcc> = Vec::new();
        accumulate_tool_call(
            &ToolCallDelta {
                index: 0,
                id: Some("call_a".into()),
                kind: Some("function".into()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("foo".into()),
                    arguments: Some("{}".into()),
                }),
            },
            &mut acc,
        );
        accumulate_tool_call(
            &ToolCallDelta {
                index: 1,
                id: Some("call_b".into()),
                kind: Some("function".into()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("bar".into()),
                    arguments: Some("{\"x\":1}".into()),
                }),
            },
            &mut acc,
        );
        assert_eq!(acc.len(), 2);
        assert_eq!(acc[0].finalize().id, "call_a");
        assert_eq!(acc[1].finalize().id, "call_b");
        assert_eq!(acc[1].finalize().function.arguments, "{\"x\":1}");
    }
}
