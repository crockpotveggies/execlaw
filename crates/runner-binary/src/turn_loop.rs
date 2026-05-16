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
use anyhow::{Result, anyhow};
use execlaw_inference_api::{
    ChatMessage, ChatRequest, ChatStreamChoice, InferenceClient, ModelId, Role, ToolCall,
    ToolCallDelta, ToolCallFunction,
};
use execlaw_runner_protocol::{RunnerToServer, ToolCallResult, ToolOutcome, TurnRequest};
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use tokio::sync::mpsc;

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

/// Belt-and-suspenders ceiling on tool-call rounds. The supervisor's
/// per-turn cap arrives in `TurnRequest.max_tool_rounds`; we clamp it
/// to this ceiling so a misconfigured server (or an older supervisor
/// that doesn't set the field — `serde(default)` yields 16, never
/// higher than this ceiling) can't pin the runner in an unbounded
/// loop. The runner enforces `min(req.max_tool_rounds,
/// RUNNER_MAX_TOOL_ROUNDS)`.
pub const RUNNER_MAX_TOOL_ROUNDS: u32 = 24;

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
    // 2026-05-15 — vision support. When the supervisor passed image
    // data URLs in `user_image_urls`, build an OpenAI vision content
    // array via `ChatMessage::user_with_images`. Otherwise fall
    // through to the text-only path (the historical shape; every
    // text-only turn keeps an identical wire encoding).
    if req.user_image_urls.is_empty() {
        messages.push(ChatMessage {
            role: Role::User,
            content: Some(execlaw_inference_api::MessageContent::Text(user_text)),
            tool_call_id: None,
            name: None,
            tool_calls: vec![],
        });
    } else {
        messages.push(ChatMessage::user_with_images(
            user_text,
            req.user_image_urls.iter().cloned(),
        ));
    }

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
    // 2026-05-16 — per-turn cap: take the supervisor's value and
    // clamp to the hard ceiling. `req.max_tool_rounds` defaults to 16
    // via `serde(default)` when an older supervisor omits the field.
    let effective_max_rounds = req.max_tool_rounds.min(RUNNER_MAX_TOOL_ROUNDS);
    // 2026-05-12 — turn-timing instrumentation (runner-mediated
    // path, streaming). Same `agent::turn_timing` target as the
    // in-process executor so a downstream log aggregator can union
    // both paths without per-source filters. All measurements are
    // wall-clock on the monotonic Instant clock; absolute values
    // aren't comparable across processes but deltas are.
    let turn_started_at = std::time::Instant::now();
    let conversation_id_for_log = req.conversation_id.clone();
    tracing::debug!(
        target: "agent::turn_timing",
        conversation_id = %conversation_id_for_log,
        tool_catalog_count = tools.as_ref().map(|t| t.len()).unwrap_or(0),
        history_msg_count = messages.len(),
        "turn starting (runner-mediated, streaming)"
    );

    loop {
        if round >= effective_max_rounds {
            return Err(anyhow!(
                "runner hit max_tool_rounds={effective_max_rounds} (ceiling {RUNNER_MAX_TOOL_ROUNDS}); aborting turn"
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
            // 2026-05-16 — explicit `tool_choice: "auto"` when tools
            // are advertised. OpenAI spec says omission defaults to
            // "auto", but vLLM versions ≤ 0.6 silently fall into a
            // no-tools fast path on omission; passing it explicitly
            // ensures the `--enable-auto-tool-choice` server flag
            // engages every round. `None` when there are no tools
            // (a `"none"` tool_choice value is the spec-correct
            // fallback but some llama.cpp servers reject it).
            tool_choice: if tools.is_some() {
                Some(serde_json::Value::String("auto".to_owned()))
            } else {
                None
            },
            // 2026-05-16 — engage a guided-decoding backend for
            // tool-bearing requests. On vLLM ≥ 0.7 with
            // `--enable-auto-tool-choice`, this constrains the
            // chosen tool's `function.arguments` to match its JSON
            // schema at decode time — the failure class behind the
            // "Expecting ',' delimiter" 400s we were seeing on
            // Signal-channel chart turns. Older vLLM versions and
            // non-vLLM backends ignore the field.
            //
            // Env-controlled so operators can A/B-diagnose without
            // a redeploy: outlines compiles a grammar from the
            // schema on first use and that compile pass can stall
            // vLLM's prefill for a complex schema (chart.render's
            // nested series/points). Set
            // `EXECLAW_GUIDED_DECODING_BACKEND=""` to disable, or
            // `=xgrammar`/`=lm-format-enforcer` to swap backends.
            // Unset = default `outlines`.
            guided_decoding_backend: resolve_guided_decoding_backend(tools.is_some()),
        };
        // Per-round timing. For STREAMING (which this is) the
        // useful splits are:
        //   * `open_stream_ms` — request send + 200 OK headers
        //     (a network/TLS/queue cost, mostly stable per call).
        //   * `first_chunk_ms` — headers → first body chunk
        //     (vLLM's prefill latency; this is the time the model
        //     spent processing the prompt before producing its
        //     first generated token).
        //   * `stream_total_ms` — open → final chunk (covers
        //     prefill + the full decode of all output tokens).
        // The (chunks, text_acc.len()) pair lets the operator
        // back into a rough decode tps after the fact.
        let round_started_at = std::time::Instant::now();
        let stream_result = client.chat_completions_stream(&chat_req).await;
        let mut stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                let open_stream_ms = round_started_at.elapsed().as_millis() as u64;
                let wrapped = anyhow::Error::new(e).context("opening inference stream");
                let chain = format!("{wrapped:#}");
                emit_failure_snapshot(FailureSnapshot {
                    turn_id: &req.turn_id,
                    conversation_id: &req.conversation_id,
                    round,
                    model: &model_id,
                    chat_req: &chat_req,
                    open_stream_ms,
                    first_chunk_ms: 0,
                    decode_ms: 0,
                    stream_total_ms: open_stream_ms,
                    chunks_received: 0,
                    chunks_per_sec: 0,
                    text_acc: "",
                    finish_reason: None,
                    error_chain: &chain,
                    failure_kind: "stream_open_failure",
                    tool_call_deltas_received: 0,
                    guided_decoding_backend: chat_req.guided_decoding_backend.as_deref(),
                });
                return Err(wrapped);
            }
        };
        let open_stream_ms = round_started_at.elapsed().as_millis() as u64;

        let mut text_acc = String::new();
        let mut tool_calls: Vec<ToolCallAcc> = Vec::new();
        let mut finish_reason: Option<String> = None;
        let mut first_chunk_at: Option<std::time::Instant> = None;
        let mut chunk_count: u64 = 0;
        let mut last_chunk_at = std::time::Instant::now();
        // Captured on mid-stream errors so the round-summary log
        // below can fire BEFORE we bubble the error up.
        let mut stream_err: Option<anyhow::Error> = None;

        // 2026-05-16 — idle-watchdog. vLLM's prefill on a long
        // prompt OR outlines grammar-compilation on a complex tool
        // schema can both produce 10-60s of dead air before the
        // first chunk arrives. Without instrumentation an operator
        // sees a single "operation timed out" line at the end and
        // can't tell if the stall was at prefill (zero chunks) or
        // mid-decode (some chunks, then nothing). This watchdog
        // fires a WARN every `EXECLAW_INFERENCE_IDLE_WARN_SECS`
        // seconds of silence (default 5) with the current state —
        // operators see a heartbeat in the journal and can
        // distinguish the two failure modes by reading the trail.
        let idle_warn_secs: u64 = std::env::var("EXECLAW_INFERENCE_IDLE_WARN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);
        let mut idle_interval =
            tokio::time::interval(std::time::Duration::from_secs(idle_warn_secs));
        idle_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Consume the immediate-first tick so the warn fires only
        // after `idle_warn_secs` of actual idleness, not at t=0.
        idle_interval.tick().await;

        loop {
            if cancel.load(Ordering::SeqCst) {
                was_cancelled = true;
                break;
            }
            tokio::select! {
                biased;
                maybe_chunk = stream.next() => {
                    let Some(chunk) = maybe_chunk else { break; };
                    match chunk {
                        Ok(c) => {
                            if first_chunk_at.is_none() {
                                first_chunk_at = Some(std::time::Instant::now());
                            }
                            last_chunk_at = std::time::Instant::now();
                            chunk_count = chunk_count.saturating_add(1);
                            if !c.model.is_empty() {
                                model_id = c.model.clone();
                            }
                            for ch in &c.choices {
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
                        Err(e) => {
                            stream_err = Some(
                                anyhow::Error::new(e).context("reading inference stream chunk"),
                            );
                            break;
                        }
                    }
                }
                _ = idle_interval.tick() => {
                    let idle_ms = last_chunk_at.elapsed().as_millis() as u64;
                    let first_chunk_seen = first_chunk_at.is_some();
                    tracing::warn!(
                        target: "runner::turn_loop",
                        turn_id = %req.turn_id,
                        conversation_id = %req.conversation_id,
                        round,
                        idle_ms,
                        chunks_so_far = chunk_count,
                        text_chars_so_far = text_acc.chars().count(),
                        first_chunk_seen,
                        stall_phase = if first_chunk_seen { "decode" } else { "prefill" },
                        "inference stream idle — no chunks arrived in the last interval"
                    );
                }
            }
        }
        drop(stream);
        let stream_total_ms = round_started_at.elapsed().as_millis() as u64;
        let first_chunk_ms = first_chunk_at
            .map(|t| t.duration_since(round_started_at).as_millis() as u64)
            .unwrap_or(0);
        let decode_ms = if first_chunk_at.is_some() {
            stream_total_ms.saturating_sub(first_chunk_ms)
        } else {
            0
        };
        // Rough chunks-per-second over the decode window. Floor at
        // 1ms so a single-chunk stream doesn't divide by zero.
        let chunks_per_sec = if decode_ms > 0 {
            (chunk_count as f64 * 1000.0 / decode_ms as f64) as u64
        } else {
            0
        };
        let errored = stream_err.is_some();
        // 2026-05-16 — round summary fires on BOTH success and
        // error paths. Pre-fix, mid-stream failures bubbled out
        // via `?` before the summary log could run, so an operator
        // tracking down a timeout saw the error chain but had no
        // timing context. The `errored` flag + the split-phase
        // numbers (open_stream / first_chunk / decode) tell the
        // story: "we got the first chunk in 200ms, then 28s of
        // decode produced 4 chunks before vLLM dropped" reads
        // very differently from "we never got a first chunk in
        // 30s, vLLM stalled at prefill."
        tracing::debug!(
            target: "agent::turn_timing",
            conversation_id = %conversation_id_for_log,
            round,
            open_stream_ms,
            first_chunk_ms,
            decode_ms,
            stream_total_ms,
            chunks_received = chunk_count,
            chunks_per_sec,
            text_chars = text_acc.chars().count(),
            tool_calls_accumulated = tool_calls.len(),
            finish_reason = ?finish_reason,
            errored,
            "round stream complete"
        );
        if let Some(e) = stream_err {
            // Surface the error AFTER the timing summary fired
            // so an operator scanning the journal sees both
            // ordering and timing context. The snapshot record
            // below bundles everything (timings, partial text,
            // request body, error chain) into a single grep-able
            // log line at target `runner::turn_failure_snapshot`.
            let chain = format!("{e:#}");
            emit_failure_snapshot(FailureSnapshot {
                turn_id: &req.turn_id,
                conversation_id: &conversation_id_for_log,
                round,
                model: &model_id,
                chat_req: &chat_req,
                open_stream_ms,
                first_chunk_ms,
                decode_ms,
                stream_total_ms,
                chunks_received: chunk_count,
                chunks_per_sec,
                text_acc: &text_acc,
                finish_reason: finish_reason.as_deref(),
                error_chain: &chain,
                failure_kind: "mid_stream_read_failure",
                tool_call_deltas_received: tool_calls.len(),
                guided_decoding_backend: chat_req.guided_decoding_backend.as_deref(),
            });
            return Err(e);
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

        // 2026-05-16 — diagnostic for the "model has tools but
        // chose text" failure mode (operator reports the agent
        // ignored a chart request and just typed a reply). On a
        // tool-bearing turn this is rarely intentional — the
        // common cause is over-constrained grammar
        // (`guided_decoding_backend` + tight schema) making the
        // model give up rather than emit an invalid tool call.
        // We log it at WARN so an operator chasing "why didn't
        // the agent call X" sees the event without bumping
        // verbosity. Fires only on round 1 of a turn (subsequent
        // rounds legitimately conclude with text after consuming
        // tool results).
        if round == 1 && finish != "tool_calls" && !tool_calls.is_empty() {
            // Defensive: model emitted partial tool_calls but
            // finished with `stop` / `length` instead of
            // `tool_calls`. That's also a parser/grammar issue.
            tracing::warn!(
                turn_id = %req.turn_id,
                conversation_id = %req.conversation_id,
                finish_reason = %finish,
                partial_tool_calls = tool_calls.len(),
                "model emitted partial tool_calls but finish_reason != tool_calls — \
                 inspect parser / grammar config"
            );
        }
        if round == 1
            && finish != "tool_calls"
            && tool_calls.is_empty()
            && tools.as_ref().is_some_and(|t| !t.is_empty())
        {
            tracing::warn!(
                target: "runner::turn_loop",
                turn_id = %req.turn_id,
                conversation_id = %req.conversation_id,
                finish_reason = %finish,
                tools_available = tools.as_ref().map(|t| t.len()).unwrap_or(0),
                text_chars = text_acc.chars().count(),
                text_preview = %text_acc.chars().take(400).collect::<String>(),
                "MODEL_DID_NOT_CALL_TOOL — turn finished text-only despite tools being \
                 advertised. Most likely cause: guided-decoding grammar too strict for the \
                 model to satisfy. Try EXECLAW_GUIDED_DECODING_BACKEND=\"\" (already the new \
                 default) and rebuild. Compare with /api/admin/inference/probe results."
            );
        }

        if finish == "tool_calls" && !tool_calls.is_empty() {
            // Append the assistant's tool_calls turn to the
            // history. content stays Some(text_acc) so the model
            // remembers any reasoning it emitted alongside the
            // call. tool_calls carries the structured calls the
            // model produced.
            let assistant_calls: Vec<ToolCall> =
                tool_calls.iter().map(ToolCallAcc::finalize).collect();
            // 2026-05-16 — sanitize tool_call.arguments before
            // echoing the assistant turn back into the round-N+1
            // request. Models (Qwen3.5-AWQ in particular, and only
            // under the Signal-tier system prompt — see the
            // companion fix at builtin_tools.rs::defensive_unstringify_spec_fields)
            // sometimes emit *almost-valid* JSON in `function.arguments`:
            // a stringified inner array, an extra trailing `}`, etc.
            // The tool-side defensive parser eats those fine, but
            // vLLM re-validates the assistant message on the next
            // round and rejects the request with
            //   "Expecting ',' delimiter: line 1 column N (char N-1)"
            // — a 400 originating from json.loads on the arguments
            // sub-string, not the outer body. Round-trip through
            // serde_json here: a successful parse re-emits canonical
            // JSON (whitespace + key order normalized) which vLLM
            // accepts; a failed parse falls back to `{}` so the
            // outer round stays valid and the model sees the tool's
            // error tool_result and can correct. The original args
            // are still preserved on the wire to the dispatcher via
            // `parsed_args` below (string-wrapped on parse failure),
            // so the tool still gets to attempt its own recovery.
            let sanitized_assistant_calls: Vec<ToolCall> = assistant_calls
                .iter()
                .map(|c| {
                    let canonical_args = match serde_json::from_str::<serde_json::Value>(
                        &c.function.arguments,
                    )
                    .and_then(|v| serde_json::to_string(&v))
                    {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(
                                target: "runner::turn_loop",
                                tool = %c.function.name,
                                call_id = %c.id,
                                error = %e,
                                "tool_call.arguments not valid JSON; substituting empty-object \
                                 placeholder for the round-N+1 assistant message so vLLM doesn't \
                                 400 on echo",
                            );
                            "{}".to_owned()
                        }
                    };
                    ToolCall {
                        id: c.id.clone(),
                        kind: c.kind.clone(),
                        function: ToolCallFunction {
                            name: c.function.name.clone(),
                            arguments: canonical_args,
                        },
                    }
                })
                .collect();
            messages.push(ChatMessage {
                role: Role::Assistant,
                content: if text_acc.is_empty() {
                    None
                } else {
                    Some(execlaw_inference_api::MessageContent::Text(
                        text_acc.clone(),
                    ))
                },
                tool_call_id: None,
                name: None,
                tool_calls: sanitized_assistant_calls,
            });

            // Forward each call to the supervisor and park on the
            // per-turn mailbox until the matching result arrives.
            // Out-of-order results are tolerated: we drain by
            // `call_id` until every outstanding call has resolved.
            //
            // 2026-05-16 — track which call_ids had unparseable
            // `function.arguments` (`malformed_this_round`) and
            // which ones came back as a `ToolOutcome::Err`
            // (`errored_this_round`). The intersection of those two
            // sets is what `build_malformed_args_retry_note` keys
            // off — when a call's args couldn't be parsed AND the
            // tool reported failure (because it also couldn't make
            // sense of them), inject a one-line system reminder
            // into the next round so the model knows to re-emit
            // valid JSON. This is the *next* defense layer after
            // canonicalization (which fixes the round-N+1 echo) and
            // grammar enforcement (which prevents malformation at
            // decode time on outlines-equipped vLLM).
            let mut outstanding: HashMap<String, ToolCall> = HashMap::new();
            let mut malformed_this_round: HashSet<String> = HashSet::new();
            let mut errored_this_round: HashSet<String> = HashSet::new();
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
                            malformed_this_round.insert(call.id.clone());
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
                        let content = match &result.outcome {
                            ToolOutcome::Ok { value } => serde_json::to_string(value)
                                .unwrap_or_else(|_| "\"<unrepresentable result>\"".into()),
                            ToolOutcome::Err { message } => {
                                errored_this_round.insert(result.call_id.clone());
                                serde_json::to_string(
                                    &serde_json::json!({"error": message}),
                                )
                                .unwrap_or_else(|_| "{\"error\":\"<unrepresentable\"}".into())
                            }
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
            // 2026-05-16 — surface the malformed-args reminder. Only
            // fires when a call was both unparseable AND errored on
            // the tool side; tool defensive parsers (e.g.
            // chart.render's `defensive_unstringify_spec_fields`)
            // that recover from almost-valid input never trip this.
            if let Some(note) =
                build_malformed_args_retry_note(&assistant_calls, &malformed_this_round, &errored_this_round)
            {
                tracing::info!(
                    target: "runner::turn_loop",
                    turn_id = %req.turn_id,
                    round = round,
                    "injecting malformed-args retry reminder for next round"
                );
                messages.push(ChatMessage::system(note));
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

    let total_ms = turn_started_at.elapsed().as_millis() as u64;
    tracing::debug!(
        target: "agent::turn_timing",
        conversation_id = %conversation_id_for_log,
        total_ms,
        tool_rounds = round.saturating_sub(1),
        assistant_text_chars = final_assistant_text.chars().count(),
        finish_reason = ?final_finish_reason,
        "turn complete (runner-mediated)"
    );

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

/// Snapshot of a failed turn — every field a postmortem needs,
/// gathered into one struct so the emitter doesn't take a 15-arg
/// signature. Built in the runner's failure-path branches; passed
/// to [`emit_failure_snapshot`] which fans out to a single
/// consolidated `tracing::error!` AND (optionally) a JSON file on
/// disk.
pub(crate) struct FailureSnapshot<'a> {
    pub turn_id: &'a str,
    pub conversation_id: &'a str,
    pub round: u32,
    pub model: &'a str,
    pub chat_req: &'a ChatRequest,
    pub open_stream_ms: u64,
    pub first_chunk_ms: u64,
    pub decode_ms: u64,
    pub stream_total_ms: u64,
    pub chunks_received: u64,
    pub chunks_per_sec: u64,
    /// Partial assistant text accumulated before the failure.
    /// Empty when the stream errored at prefill.
    pub text_acc: &'a str,
    pub finish_reason: Option<&'a str>,
    /// Anyhow `{e:#}` chain — top-level label + every `.context`
    /// frame down to the root cause.
    pub error_chain: &'a str,
    /// Short tag describing the failure mode for filtering:
    /// `"stream_open_failure"`, `"mid_stream_read_failure"`, etc.
    pub failure_kind: &'a str,
    /// 2026-05-16 — count of `tool_call_delta` frames received
    /// before the failure. Distinguishes "model emitted zero
    /// output" from "model was mid-tool-call-stream when the
    /// wire died." Without this field, `text_chars_partial=0`
    /// is ambiguous between those two very different failure
    /// modes.
    pub tool_call_deltas_received: usize,
    /// 2026-05-16 — the `guided_decoding_backend` value sent in
    /// the request (`Some("outlines")` / `Some("xgrammar")` /
    /// `None`). Surfacing it here means an operator chasing a
    /// stall doesn't have to inspect the 8 KB-truncated body
    /// preview to know whether grammar enforcement was engaged
    /// — relevant because outlines-on-complex-schemas is a known
    /// stall cause.
    pub guided_decoding_backend: Option<&'a str>,
}

/// Emit a single consolidated postmortem record for a failed turn.
///
/// Always emits one `tracing::error!` log line at target
/// `runner::turn_failure_snapshot` containing every field needed
/// to reconstruct what happened. Operator workflow:
///
/// ```sh
/// # Find recent failures
/// journalctl -u execlaw-runner --since "1 hour ago" \
///   | grep "TURN_FAILURE_SNAPSHOT"
///
/// # With a JSON tracing subscriber, pipe through jq for structure:
/// journalctl -u execlaw-runner -o cat \
///   | grep "TURN_FAILURE_SNAPSHOT" | jq
/// ```
///
/// Additionally, when env var `EXECLAW_RUNNER_DIAGNOSTICS_DIR` is
/// set, writes a JSON file at `<dir>/<turn_id>.json` with the
/// same fields plus a longer text-partial preview. The disk path
/// is the right choice when the runner container's logs rotate
/// quickly or when an operator wants to `docker cp` the snapshot
/// out for sharing. Disabled by default — no env var means no
/// disk writes, just the log line.
pub(crate) fn emit_failure_snapshot(s: FailureSnapshot<'_>) {
    let (body_preview, body_chars, body_truncated) = match serde_json::to_string(s.chat_req) {
        Ok(serialized) => {
            let preview: String = serialized.chars().take(8192).collect();
            let truncated = serialized.len() > preview.len();
            (preview, serialized.chars().count(), truncated)
        }
        Err(_) => (String::new(), 0, false),
    };
    let text_preview_partial: String = s.text_acc.chars().take(1024).collect();
    let text_chars_partial = s.text_acc.chars().count();

    // Single consolidated record. The text `TURN_FAILURE_SNAPSHOT`
    // in the message body is a grep-target so operators with
    // plaintext journals (no JSON subscriber) can still locate
    // every postmortem with one query.
    tracing::error!(
        target: "runner::turn_failure_snapshot",
        turn_id = %s.turn_id,
        conversation_id = %s.conversation_id,
        round = s.round,
        model = %s.model,
        failure_kind = %s.failure_kind,
        body_chars = body_chars,
        body_truncated = body_truncated,
        body_preview = %body_preview,
        open_stream_ms = s.open_stream_ms,
        first_chunk_ms = s.first_chunk_ms,
        decode_ms = s.decode_ms,
        stream_total_ms = s.stream_total_ms,
        chunks_received = s.chunks_received,
        chunks_per_sec = s.chunks_per_sec,
        text_chars_partial,
        text_preview_partial = %text_preview_partial,
        tool_call_deltas_received = s.tool_call_deltas_received,
        guided_decoding_backend = ?s.guided_decoding_backend,
        finish_reason = ?s.finish_reason,
        error_chain = %s.error_chain,
        "TURN_FAILURE_SNAPSHOT — single record for postmortem"
    );

    // Optional disk archive. Env var off → no I/O, no disk
    // pressure, no surprises. Env var set → operator opted in
    // and accepts that runner failures will write files.
    if let Ok(dir) = std::env::var("EXECLAW_RUNNER_DIAGNOSTICS_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            // Sanitize turn_id (it's UUID-like but defensive
            // against future ID shapes that might include `/`).
            let safe_id: String = s
                .turn_id
                .chars()
                .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                .collect();
            let path = format!("{dir}/{safe_id}.json");
            // Full text in the disk archive (the log preview is
            // capped at 1 KiB; disk has no preview cap because
            // operators inspecting individual files benefit from
            // the full partial).
            let snapshot = serde_json::json!({
                "turn_id": s.turn_id,
                "conversation_id": s.conversation_id,
                "round": s.round,
                "model": s.model,
                "failure_kind": s.failure_kind,
                "timings": {
                    "open_stream_ms": s.open_stream_ms,
                    "first_chunk_ms": s.first_chunk_ms,
                    "decode_ms": s.decode_ms,
                    "stream_total_ms": s.stream_total_ms,
                    "chunks_per_sec": s.chunks_per_sec,
                },
                "chunks_received": s.chunks_received,
                "text_chars_partial": text_chars_partial,
                "text_partial": s.text_acc,
                "tool_call_deltas_received": s.tool_call_deltas_received,
                "guided_decoding_backend": s.guided_decoding_backend,
                "finish_reason": s.finish_reason,
                "error_chain": s.error_chain,
                "request_body": {
                    "chars": body_chars,
                    "truncated": body_truncated,
                    "preview": body_preview,
                },
            });
            match std::fs::create_dir_all(dir)
                .and_then(|_| std::fs::write(&path, snapshot.to_string()))
            {
                Ok(_) => tracing::info!(
                    target: "runner::turn_failure_snapshot",
                    path = %path,
                    "wrote failure snapshot to disk",
                ),
                Err(e) => tracing::warn!(
                    target: "runner::turn_failure_snapshot",
                    path = %path,
                    error = %e,
                    "failed to write disk snapshot; structured log still emitted above",
                ),
            }
        }
    }
}

/// Resolve which guided-decoding backend (if any) to send to vLLM
/// for tool-bearing requests.
///
/// 2026-05-16 update — **default flipped to `None`**. The previous
/// default of `"outlines"` over-constrained quantized Qwen3.5-AWQ
/// in combination with our tightened tool schemas: when the model
/// couldn't satisfy the grammar (e.g. a `required` field it
/// wasn't sure how to populate, or `additionalProperties: false`
/// on a deeply-nested schema like `chart.render`), it fell back
/// to plain text and didn't call the tool at all. That's a worse
/// outcome than "occasional malformed JSON" — the canonicalization
/// + retry-reminder layers already absorb malformations on the
/// re-issue, but they can't recover a turn the model never even
/// attempted as a tool call.
///
/// Operators wanting strict argument validity opt in via env:
///
/// ```bash
/// EXECLAW_GUIDED_DECODING_BACKEND=outlines        # or
/// EXECLAW_GUIDED_DECODING_BACKEND=xgrammar        # newer vLLM, faster compile
/// EXECLAW_GUIDED_DECODING_BACKEND=lm-format-enforcer
/// ```
///
/// Empty / whitespace value is equivalent to unset (no backend).
///
///   * `tools_present == false` → always `None`. Without tools
///     there's no schema to enforce; setting the field would be
///     a no-op and the env override shouldn't engage guided
///     decoding on plain conversation turns.
pub(crate) fn resolve_guided_decoding_backend(tools_present: bool) -> Option<String> {
    if !tools_present {
        return None;
    }
    let raw = std::env::var("EXECLAW_GUIDED_DECODING_BACKEND").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// 2026-05-16 — build a one-line system reminder when the previous
/// round contained tool_calls whose `function.arguments` couldn't
/// be parsed as JSON *and* whose tool execution returned an error.
/// Returns `None` when no call satisfies both conditions — that's
/// the common case and the `continue` path stays empty.
///
/// Why both conditions:
///   * Args-parse failure alone isn't enough — many tools have
///     defensive parsers (e.g.
///     `chart.render::defensive_unstringify_spec_fields`) that
///     recover from "almost-valid" JSON. If the tool succeeded,
///     telling the model "your last call was malformed" would
///     contradict the successful `tool_result` it's about to read.
///   * Tool-error alone isn't enough — a `ToolOutcome::Err` can
///     mean "valid args, the operation failed" (missing API key,
///     network timeout, no such record). Retrying with the same
///     args won't help; the model needs the actual error message
///     in the tool_result, not a generic "your JSON was bad" note.
///
/// The intersection — malformed JSON AND tool failure — is the
/// specific failure mode this layer targets: the model emitted
/// shape the tool couldn't even decode, the tool errored out, and
/// the model needs an explicit prod to fix its formatting on the
/// retry rather than reading the cryptic error as a substantive
/// failure.
pub(crate) fn build_malformed_args_retry_note(
    assistant_calls: &[ToolCall],
    malformed_call_ids: &HashSet<String>,
    errored_call_ids: &HashSet<String>,
) -> Option<String> {
    let mut offending: Vec<&str> = assistant_calls
        .iter()
        .filter(|c| malformed_call_ids.contains(&c.id) && errored_call_ids.contains(&c.id))
        .map(|c| c.function.name.as_str())
        .collect();
    if offending.is_empty() {
        return None;
    }
    // Stable wording — dedup names so a tool called twice with bad
    // args doesn't get listed twice (preserve first occurrence).
    let mut seen: HashSet<&str> = HashSet::new();
    offending.retain(|name| seen.insert(*name));
    let tool_list = offending.join(", ");
    Some(format!(
        "Your previous call to `{tool_list}` had malformed JSON in `function.arguments`, so the \
         tool could not execute. Retry the call with valid JSON that matches the tool's schema \
         exactly — arrays must be real arrays (not strings containing JSON), and there must be \
         no extra trailing braces or characters."
    ))
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

    // ---- build_malformed_args_retry_note ---------------------------
    //
    // Pin the contract that drives the runner's next-round retry
    // reminder: it fires iff a call_id appears in BOTH the malformed-
    // args set AND the errored-result set. Tool-side defensive
    // recovery (parse-failed but tool succeeded) and tool-side
    // failures with valid args (parse-ok but tool errored) both
    // produce `None` here — see the helper's doc comment.

    fn tc(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: name.into(),
                arguments: "{}".into(),
            },
        }
    }

    #[test]
    fn retry_note_fires_when_call_is_both_malformed_and_errored() {
        let calls = vec![tc("call_a", "chart.render")];
        let malformed = HashSet::from(["call_a".to_string()]);
        let errored = HashSet::from(["call_a".to_string()]);
        let note = build_malformed_args_retry_note(&calls, &malformed, &errored)
            .expect("note should be present");
        assert!(
            note.contains("chart.render"),
            "note must name the offending tool; got: {note}",
        );
        assert!(
            note.to_lowercase().contains("malformed"),
            "note must mention malformed JSON; got: {note}",
        );
    }

    #[test]
    fn retry_note_silent_when_malformed_but_tool_recovered() {
        // The chart.render path: args were stringified-array, the
        // tool's defensive parser unstringified them and rendered
        // successfully. No retry needed — the model gets the
        // successful tool_result and continues normally.
        let calls = vec![tc("call_a", "chart.render")];
        let malformed = HashSet::from(["call_a".to_string()]);
        let errored = HashSet::new();
        assert!(
            build_malformed_args_retry_note(&calls, &malformed, &errored).is_none(),
            "must not fire on parse-failed but tool-succeeded calls",
        );
    }

    #[test]
    fn retry_note_silent_when_args_valid_but_tool_errored() {
        // Substantive tool failure (missing API key, no such record,
        // upstream timeout). Retrying with the same args won't fix
        // it — the model needs the real error text, not a generic
        // "your JSON was bad" reminder that would be misleading.
        let calls = vec![tc("call_a", "calendar.create_event")];
        let malformed = HashSet::new();
        let errored = HashSet::from(["call_a".to_string()]);
        assert!(
            build_malformed_args_retry_note(&calls, &malformed, &errored).is_none(),
            "must not fire on tool-errored but parse-succeeded calls",
        );
    }

    #[test]
    fn retry_note_lists_every_offending_tool_dedup_preserved() {
        // Two distinct tools both malformed+errored AND one
        // duplicate call — the note should list each tool name
        // exactly once, in first-occurrence order.
        let calls = vec![
            tc("call_a", "chart.render"),
            tc("call_b", "calendar.create_event"),
            tc("call_c", "chart.render"), // duplicate name, different id
        ];
        let malformed = HashSet::from([
            "call_a".to_string(),
            "call_b".to_string(),
            "call_c".to_string(),
        ]);
        let errored = HashSet::from([
            "call_a".to_string(),
            "call_b".to_string(),
            "call_c".to_string(),
        ]);
        let note = build_malformed_args_retry_note(&calls, &malformed, &errored).unwrap();
        assert!(note.contains("chart.render"));
        assert!(note.contains("calendar.create_event"));
        // Dedup check: chart.render appears once, not twice.
        assert_eq!(
            note.matches("chart.render").count(),
            1,
            "duplicate tool names must dedup; got: {note}",
        );
    }

    #[test]
    fn retry_note_silent_when_intersection_is_empty() {
        // Disjoint sets — different calls failed for different
        // reasons. No call satisfies BOTH conditions, so no note.
        let calls = vec![tc("call_a", "x"), tc("call_b", "y")];
        let malformed = HashSet::from(["call_a".to_string()]); // x: bad JSON, tool ok
        let errored = HashSet::from(["call_b".to_string()]); // y: ok JSON, tool failed
        assert!(build_malformed_args_retry_note(&calls, &malformed, &errored).is_none());
    }
}
