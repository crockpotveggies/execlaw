//! AskAgent invocation surface for the automation runtime (M3).
//!
//! The runtime calls into an [`AgentInvoker`] to execute an `AskAgent`
//! node. The invoker is the seam that lets us:
//!
//!   * Run the real LLM via [`InferenceAgentInvoker`] in production.
//!   * Run scripted, deterministic replies via [`StubAgentInvoker`]
//!     in tests — no LLM, no network, no flakiness.
//!
//! Both are wrapped in an [`AutomationsAgentPool`] that bounds
//! concurrent in-flight calls (locked decision: default size 1, with
//! the pool abstraction in place so the operator can widen later
//! without refactoring).
//!
//! Locked invariants this module enforces:
//!
//!   * **Vision capability check** — attachments + a text-only model
//!     → fail-fast with [`AskAgentError::VisionRequiredButTextOnlyModel`].
//!     The error message includes guidance about the Settings page so
//!     the operator can pick a vision model.
//!   * **Exactly-one exit tool** — the agent MUST call exactly one of
//!     the synthesized exit tools. Multi-tool calls in one turn:
//!     first wins, rest logged. No call within `max_turns`: fail.
//!   * **Trust posture** — the invoker is the *only* path that talks
//!     to the inference backend on behalf of an automation. There's
//!     no implicit conversation history, no per-automation principal,
//!     no plugin tool exposure beyond what the flow author opted into
//!     via `reasoning_tools`.
//!
//! M3a scope: `max_turns` is effectively capped at 1 (single-shot —
//! no reasoning tools wired through to the model). The multi-turn
//! loop with intermediate tool execution is a follow-up; the API
//! surface is sized for it (the `effective_max_turns` field is read
//! and surfaced in errors so a `max_turns > 1` flow degrades
//! observably rather than silently dropping the extra rounds).

use crate::inference_metrics::{InferenceConsumer, InferenceMetrics};
use crate::inference_resolver::InferenceResolver;
use async_trait::async_trait;
use execlaw_core::Database;
use execlaw_core::automations::AskAgentConfig;
use execlaw_core::backends::BackendPurpose;
use execlaw_inference_api::{
    ChatMessage, ChatRequest, FunctionDecl, InferenceClient, ModelId, ToolCall, ToolDeclaration,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing::warn;

/// Outcome of a successful `AskAgent` invocation: which exit tool the
/// agent picked, and what arguments it filled in. The runtime stores
/// this as the node's output (under `<node_id>.tool` and
/// `<node_id>.args`) so downstream edges' `when` clauses can route on
/// the tool name and downstream nodes can read the args.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExitToolCall {
    pub name: String,
    pub args: serde_json::Value,
}

/// Default pool concurrency. Per the locked-decision in v3.2: "single
/// shared automations runner, pool=1". The pool abstraction is in
/// place so the operator can widen it later by passing a different
/// value to [`AutomationsAgentPool::with_concurrency`].
pub const DEFAULT_POOL_CONCURRENCY: usize = 1;

/// Categorized failure modes. Each variant lands in the run's
/// step_traces with an error message tuned for the SPA's runs drawer
/// — operators should be able to read it and know what to do.
#[derive(Debug, Error)]
pub enum AskAgentError {
    #[error("AskAgent: no inference backend configured. Configure a model in Settings > Backends.")]
    NoLlmConfigured,
    #[error(
        "AskAgent: this automation requires vision (image attachments present) but the configured model is text-only. Configure a vision-capable model in Settings > Backends, or remove the attachments."
    )]
    VisionRequiredButTextOnlyModel { model_id: String },
    #[error(
        "AskAgent: the agent did not call any exit tool within {max_turns} turn(s). Authors should make the prompt unambiguously require one of: {tool_names}."
    )]
    NoExitToolCalled { max_turns: u32, tool_names: String },
    #[error("AskAgent: the agent called an unknown tool '{name}' (not one of: {valid})")]
    UnknownExitToolCalled { name: String, valid: String },
    #[error("AskAgent: agent tool-call arguments not valid JSON: {0}")]
    BadToolCallArgs(String),
    #[error("AskAgent: inference backend error: {0}")]
    LlmFailure(String),
    #[error("AskAgent: config invalid: {0}")]
    ConfigInvalid(String),
    #[error("AskAgent: pool unavailable (shut down)")]
    PoolUnavailable,
}

/// Request payload for an invocation. The pool / invoker pair owns
/// all the cross-cutting concerns (capability check, trust, model
/// resolution); the request is just "what does the flow author want".
#[derive(Debug, Clone)]
pub struct AskAgentRequest {
    pub config: AskAgentConfig,
    /// Flow-run id (M6-D streaming). When `Some`, the invoker emits
    /// `FlowChannelEvent::AgentTextDelta` and friends so the
    /// `/api/automations/flow-runs/{run_id}/events` SSE consumer
    /// receives live token deltas. `None` disables flow-trace
    /// streaming (test invocations, ad-hoc CLI use).
    pub run_id: Option<String>,
    /// Source AskAgent node id — used as the `node_id` field on the
    /// emitted FlowChannelEvents so SPA renderers can attribute the
    /// stream to the right tile.
    pub node_id: Option<String>,
    /// Conversation id when the trigger envelope's origin is
    /// `ChatAppend{conversation_id}` (M6-D / M6-E). The invoker
    /// mirrors text deltas to `UiEvent::ChatTokenDelta` so the chat
    /// UI receives live text alongside the flow-trace stream.
    pub conversation_id: Option<String>,
    /// Conversation history for chat-context AskAgent runs (M6-E).
    /// The runtime loads prior `user_msg` + `model_turn` events from
    /// the conversation's event log and hands them in already
    /// truncated to the token budget. The invoker prepends them to
    /// `request.messages` between the system prompt and the
    /// cfg.prompt-derived user message. Empty for non-chat flows.
    pub history: Vec<HistoryEntry>,
}

/// One prior turn projection — role + text, the same shape the
/// `execlaw_core::history_budget` policy hands the chat path. Kept
/// in this crate so AskAgentRequest doesn't drag the wider
/// `history_budget` type into the AgentInvoker trait surface.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub role: HistoryRole,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryRole {
    User,
    Assistant,
}

impl AskAgentRequest {
    /// Build a config-only request — used by tests and the few
    /// non-flow callers that don't need streaming. Equivalent to the
    /// pre-M6-D shape.
    pub fn from_config(config: AskAgentConfig) -> Self {
        Self {
            config,
            run_id: None,
            node_id: None,
            conversation_id: None,
            history: Vec::new(),
        }
    }
}

/// Behind-the-trait contract. Implementations are responsible for:
///   * Detecting "no LLM" and returning `NoLlmConfigured`.
///   * Running the model-capability check when `config.attachments`
///     is non-empty.
///   * Synthesizing exit tools into the model's tool palette.
///   * Parsing the model's response for the first exit-tool call.
#[async_trait]
pub trait AgentInvoker: Send + Sync {
    async fn invoke(&self, req: &AskAgentRequest) -> Result<ExitToolCall, AskAgentError>;
}

/// Bounded-concurrency wrapper. Used by both production and test
/// configurations so the runtime always sees the same shape.
pub struct AutomationsAgentPool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    permits: Semaphore,
    invoker: Arc<dyn AgentInvoker>,
}

impl AutomationsAgentPool {
    pub fn new(invoker: Arc<dyn AgentInvoker>) -> Self {
        Self::with_concurrency(invoker, DEFAULT_POOL_CONCURRENCY)
    }

    pub fn with_concurrency(invoker: Arc<dyn AgentInvoker>, concurrency: usize) -> Self {
        Self {
            inner: Arc::new(PoolInner {
                permits: Semaphore::new(concurrency.max(1)),
                invoker,
            }),
        }
    }

    pub async fn invoke(&self, req: &AskAgentRequest) -> Result<ExitToolCall, AskAgentError> {
        let _permit = self
            .inner
            .permits
            .acquire()
            .await
            .map_err(|_| AskAgentError::PoolUnavailable)?;
        self.inner.invoker.invoke(req).await
    }

    /// Clone of the underlying handle for sharing across the runtime.
    pub fn handle(&self) -> AutomationsAgentPool {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl Clone for AutomationsAgentPool {
    fn clone(&self) -> Self {
        self.handle()
    }
}

// ---------------------------------------------------------------------------
// StubAgentInvoker — deterministic replies for tests.
// ---------------------------------------------------------------------------

/// Test-only invoker. Constructed with a fixed [`ExitToolCall`] (or an
/// [`AskAgentError`]) and returns it for every `invoke`. Lets the
/// runtime's E2E tests assert "this automation runs end-to-end with
/// THIS exit-tool outcome" without standing up an HTTP server.
#[derive(Clone)]
pub struct StubAgentInvoker {
    response: Arc<Result<ExitToolCall, String>>,
}

impl StubAgentInvoker {
    pub fn ok(call: ExitToolCall) -> Self {
        Self {
            response: Arc::new(Ok(call)),
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            response: Arc::new(Err(msg.into())),
        }
    }
}

#[async_trait]
impl AgentInvoker for StubAgentInvoker {
    async fn invoke(&self, _req: &AskAgentRequest) -> Result<ExitToolCall, AskAgentError> {
        match &*self.response {
            Ok(c) => Ok(c.clone()),
            Err(m) => Err(AskAgentError::LlmFailure(m.clone())),
        }
    }
}

// ---------------------------------------------------------------------------
// InferenceAgentInvoker — production path. Hits the real LLM.
// ---------------------------------------------------------------------------

/// Production invoker — resolves the standard inference backend per
/// call (so a Settings change takes effect on the next AskAgent
/// dispatch without restart) and issues a single chat-completion
/// request.
///
/// **M3a scope: single-turn.** The model is offered the exit_tools
/// palette and expected to call one of them on the first pass.
/// Multi-turn (where the agent can call reasoning_tools, get results
/// back, and decide later) lands in a follow-up — the API surface
/// here is sized for it but the loop body is not yet written.
#[derive(Clone)]
pub struct InferenceAgentInvoker {
    db: Database,
    inference: Arc<InferenceResolver>,
    metrics: InferenceMetrics,
    /// UiEvent bus for mirroring text deltas to chat-context
    /// conversations (M6-D). When set + the request carries a
    /// `conversation_id`, each chunk is also published as
    /// `UiEvent::ChatTokenDelta` so the chat UI sees live text the
    /// same way the legacy chat path delivers it.
    events: Option<crate::events::EventBus>,
}

impl InferenceAgentInvoker {
    /// Construct with a fresh metrics handle. Production wires the
    /// shared AppState `inference_metrics` here via [`new_with_metrics`]
    /// so the `/admin/inference` snapshot endpoint sees the
    /// `Automations` consumer slice; this default exists so tests
    /// don't need to plumb metrics through every fixture.
    pub fn new(db: Database, inference: Arc<InferenceResolver>) -> Self {
        Self::new_with_metrics(db, inference, InferenceMetrics::new())
    }

    pub fn new_with_metrics(
        db: Database,
        inference: Arc<InferenceResolver>,
        metrics: InferenceMetrics,
    ) -> Self {
        Self {
            db,
            inference,
            metrics,
            events: None,
        }
    }

    /// Wire the UiEvent bus so streaming text deltas to ChatAppend
    /// origins reach the chat UI's WebSocket subscribers.
    pub fn with_events(mut self, bus: crate::events::EventBus) -> Self {
        self.events = Some(bus);
        self
    }
}

#[async_trait]
impl AgentInvoker for InferenceAgentInvoker {
    async fn invoke(&self, req: &AskAgentRequest) -> Result<ExitToolCall, AskAgentError> {
        // Routing precedence when attachments are present (M5 vision):
        //
        //   1. If `BackendPurpose::Vision` resolves to a backend row,
        //      use it. The operator opted in by configuring the row;
        //      we trust them and skip the model-id heuristic entirely.
        //   2. Else, resolve `Standard` and apply the heuristic check
        //      from M3a — vision-required + text-only-by-name fails
        //      fast with the operator-actionable message.
        //
        // No attachments: always Standard, no capability check.
        let resolved = if req.config.attachments.is_empty() {
            self.inference
                .resolve(&self.db, BackendPurpose::Standard)
                .ok_or(AskAgentError::NoLlmConfigured)?
        } else {
            // Vision routing: only honor a Vision row that actually
            // came from `config_backends` (`source == "db"`). The
            // resolver's `bootstrap_resolved` fallback would otherwise
            // route Vision through the Standard bootstrap URL, which
            // defeats the purpose — the operator's "I have a vision
            // model" intent only exists when a real DB row is present.
            let vision = self
                .inference
                .resolve(&self.db, BackendPurpose::Vision)
                .filter(|r| r.source == "db");
            match vision {
                Some(v) => v,
                None => {
                    let standard = self
                        .inference
                        .resolve(&self.db, BackendPurpose::Standard)
                        .ok_or(AskAgentError::NoLlmConfigured)?;
                    if !model_id_is_vision_capable(&standard.model_id) {
                        return Err(AskAgentError::VisionRequiredButTextOnlyModel {
                            model_id: standard.model_id.clone(),
                        });
                    }
                    standard
                }
            }
        };
        // M5 — wrap the chat-completions call with the metrics
        // observer so `/admin/inference` can attribute load to the
        // Automations consumer.
        let stream_ctx = StreamingContext {
            events: self.events.as_ref(),
            conversation_id: req.conversation_id.as_deref(),
        };
        self.metrics
            .observe(InferenceConsumer::Automations, async {
                do_invoke(
                    &resolved.client,
                    &resolved.model_id,
                    &req.config,
                    &req.history,
                    stream_ctx,
                )
                .await
            })
            .await
    }
}

/// Bundle of optional broadcast/publish targets passed into
/// `do_invoke` so the streaming-text callback can fan deltas out to
/// the right places. Borrowed view — the invoker owns the hub/bus.
#[derive(Clone, Copy)]
struct StreamingContext<'a> {
    events: Option<&'a crate::events::EventBus>,
    conversation_id: Option<&'a str>,
}

/// Heuristic — is the model id one we recognize as vision-capable?
/// Conservative: false-negative is fine (operator gets a clear
/// failure message); false-positive would let an automation try to
/// send images to a text model and hit a 400 from the backend.
///
/// Pattern based on the v3.2 locked-decision model list (Qwen3.5/3.6
/// VL, Pixtral, LLaVA, Llama 3.2 Vision) plus the project memory's
/// reference to Qwen2.5-VL as the candidate vision swap.
fn model_id_is_vision_capable(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        "vl",       // qwen2.5-vl, qwen3.5-vl, qwen3-vl
        "vision",   // llama-3.2-vision
        "llava",    // llava-*
        "pixtral",  // pixtral-12b
        "internvl", // internvl-*
        "phi-3.5-vision",
        "phi-3-vision",
    ];
    PATTERNS.iter().any(|p| lower.contains(p))
}

async fn do_invoke(
    client: &InferenceClient,
    model_id: &str,
    cfg: &AskAgentConfig,
    history: &[HistoryEntry],
    ctx: StreamingContext<'_>,
) -> Result<ExitToolCall, AskAgentError> {
    let exit_names: Vec<String> = cfg.exit_tools.iter().map(|t| t.name.clone()).collect();
    let exit_names_csv = exit_names.join(", ");
    let system_prompt = format!(
        "You are an automation agent. You MUST call EXACTLY ONE of the tools \
         provided ({}) to terminate this turn. Do not produce a free-text reply \
         without calling a tool. Pick the tool that best matches the user message; \
         if you are uncertain, pick the most conservative one.",
        exit_names_csv
    );
    let user_msg = if cfg.attachments.is_empty() {
        ChatMessage::user(cfg.prompt.clone())
    } else {
        ChatMessage::user_with_images(cfg.prompt.clone(), cfg.attachments.iter().cloned())
    };
    // M6-E — prepend conversation history when the runtime hands us
    // one. Empty for non-chat flows; for chat-context flows the
    // runtime fetches replay_since(EventSeq(0)) on the trigger's
    // conversation_id and applies `history_budget::truncate_to_budget`
    // before passing in, so we just translate role → ChatMessage.
    let mut history_msgs: Vec<ChatMessage> = Vec::with_capacity(history.len());
    for h in history {
        match h.role {
            HistoryRole::User => history_msgs.push(ChatMessage::user(h.text.clone())),
            HistoryRole::Assistant => history_msgs.push(ChatMessage::assistant(h.text.clone())),
        }
    }
    let tools: Vec<ToolDeclaration> = cfg
        .exit_tools
        .iter()
        .map(|t| ToolDeclaration {
            kind: "function".into(),
            function: FunctionDecl {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.args_schema.clone(),
            },
        })
        .collect();
    let mut messages: Vec<ChatMessage> = Vec::with_capacity(2 + history_msgs.len());
    messages.push(ChatMessage::system(system_prompt));
    messages.extend(history_msgs);
    messages.push(user_msg);
    let request = ChatRequest {
        model: ModelId(model_id.to_string()),
        messages,
        tools: Some(tools),
        // M6-D — streaming on so the SPA sees live text. The
        // chat_completions_streamed helper fans deltas into the
        // callback below + still returns the fully-assembled
        // ChatResponse (including tool_calls) so the exit-tool
        // extraction logic below is unchanged.
        stream: true,
        temperature: Some(0.2),
        // M6-D — bumped 512 → 4096 to match the legacy chat path's
        // ceiling. The old cap was a hangover from when AskAgent was
        // strictly single-shot small-text replies; in chat-context
        // (envelope.origin = ChatAppend) the agent may produce real
        // working text before terminating.
        max_tokens: Some(4096),
        chat_template_kwargs: Some(serde_json::json!({"enable_thinking": false})),
        tool_choice: Some(serde_json::json!("required")),
        // `guided_decoding_backend = Some("outlines")` is safe to
        // pass for tool-bearing requests against vLLM (per the
        // upstream comment) and harmless against backends that
        // ignore the field. The serializer skips None entirely.
        guided_decoding_backend: Some("outlines".into()),
    };
    let resp = client
        .chat_completions_streamed(&request, |delta_text| {
            // Mirror to the chat UI when this AskAgent run is wired
            // back to a conversation. The SPA's existing
            // `chat_token_delta` handler appends into the streaming
            // buffer keyed on conversation_id — exactly the same
            // contract the legacy chat path uses.
            if let (Some(bus), Some(conv_id)) = (ctx.events, ctx.conversation_id) {
                bus.publish(crate::events::UiEvent::ChatTokenDelta {
                    conversation_id: conv_id.to_owned(),
                    text: delta_text.to_owned(),
                });
            }
        })
        .await
        .map_err(|e| AskAgentError::LlmFailure(format!("{e}")))?;
    let choice = resp
        .choices
        .first()
        .ok_or_else(|| AskAgentError::LlmFailure("empty choices in chat response".into()))?;
    let tool_calls: &[ToolCall] = &choice.message.tool_calls;
    if tool_calls.is_empty() {
        return Err(AskAgentError::NoExitToolCalled {
            max_turns: cfg.effective_max_turns(),
            tool_names: exit_names_csv,
        });
    }
    if tool_calls.len() > 1 {
        warn!(
            count = tool_calls.len(),
            first = %tool_calls[0].function.name,
            "AskAgent: agent called multiple tools in one turn; taking the first, dropping the rest",
        );
    }
    let chosen = &tool_calls[0];
    if !exit_names.iter().any(|n| n == &chosen.function.name) {
        return Err(AskAgentError::UnknownExitToolCalled {
            name: chosen.function.name.clone(),
            valid: exit_names_csv,
        });
    }
    let args: serde_json::Value = if chosen.function.arguments.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(&chosen.function.arguments)
            .map_err(|e| AskAgentError::BadToolCallArgs(format!("{e}")))?
    };
    Ok(ExitToolCall {
        name: chosen.function.name.clone(),
        args,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::automations::ExitToolDef;

    fn cfg_no_attachments() -> AskAgentConfig {
        AskAgentConfig {
            prompt: "decide".into(),
            attachments: vec![],
            reasoning_tools: vec![],
            exit_tools: vec![
                ExitToolDef {
                    name: "notify".into(),
                    description: "Call when an animal is detected".into(),
                    args_schema: serde_json::json!({
                        "type": "object",
                        "properties": {"species": {"type": "string"}},
                    }),
                },
                ExitToolDef {
                    name: "ignore".into(),
                    description: "Call otherwise".into(),
                    args_schema: serde_json::json!({"type": "object"}),
                },
            ],
            max_turns: None,
        }
    }

    fn cfg_with_image() -> AskAgentConfig {
        let mut c = cfg_no_attachments();
        c.attachments = vec!["data:image/png;base64,iVBORw0KGgo".into()];
        c
    }

    #[tokio::test]
    async fn stub_invoker_returns_scripted_call() {
        let inv = StubAgentInvoker::ok(ExitToolCall {
            name: "notify".into(),
            args: serde_json::json!({"species": "cat", "confidence": 0.91}),
        });
        let out = inv
            .invoke(&AskAgentRequest::from_config(cfg_no_attachments()))
            .await
            .unwrap();
        assert_eq!(out.name, "notify");
        assert_eq!(out.args["species"], "cat");
    }

    #[tokio::test]
    async fn stub_invoker_returns_scripted_error() {
        let inv = StubAgentInvoker::err("simulated llm failure");
        let err = inv
            .invoke(&AskAgentRequest::from_config(cfg_no_attachments()))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("simulated llm failure"));
    }

    #[tokio::test]
    async fn pool_serializes_to_default_concurrency_one() {
        // Two concurrent invocations against pool=1 must run in
        // sequence. We instrument with a sleep in the stub-replacement
        // to make the serialization observable.
        struct DelayedStub {
            response: ExitToolCall,
            delay: std::time::Duration,
            in_flight: Arc<std::sync::atomic::AtomicUsize>,
            max_observed: Arc<std::sync::atomic::AtomicUsize>,
        }
        #[async_trait]
        impl AgentInvoker for DelayedStub {
            async fn invoke(&self, _req: &AskAgentRequest) -> Result<ExitToolCall, AskAgentError> {
                let now = self
                    .in_flight
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                self.max_observed
                    .fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(self.delay).await;
                self.in_flight
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                Ok(self.response.clone())
            }
        }
        let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_observed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inv: Arc<dyn AgentInvoker> = Arc::new(DelayedStub {
            response: ExitToolCall {
                name: "notify".into(),
                args: serde_json::json!({}),
            },
            delay: std::time::Duration::from_millis(50),
            in_flight: in_flight.clone(),
            max_observed: max_observed.clone(),
        });
        let pool = AutomationsAgentPool::new(inv);
        let req = AskAgentRequest::from_config(cfg_no_attachments());
        // Fire 3 in parallel.
        let p1 = pool.clone();
        let r1 = req.clone();
        let p2 = pool.clone();
        let r2 = req.clone();
        let p3 = pool.clone();
        let r3 = req.clone();
        let (a, b, c) = tokio::join!(
            tokio::spawn(async move { p1.invoke(&r1).await }),
            tokio::spawn(async move { p2.invoke(&r2).await }),
            tokio::spawn(async move { p3.invoke(&r3).await }),
        );
        a.unwrap().unwrap();
        b.unwrap().unwrap();
        c.unwrap().unwrap();
        assert_eq!(
            max_observed.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "pool=1 must serialize concurrent invocations",
        );
    }

    #[tokio::test]
    async fn pool_with_concurrency_2_allows_two_in_flight() {
        struct DelayedStub {
            in_flight: Arc<std::sync::atomic::AtomicUsize>,
            max_observed: Arc<std::sync::atomic::AtomicUsize>,
        }
        #[async_trait]
        impl AgentInvoker for DelayedStub {
            async fn invoke(&self, _req: &AskAgentRequest) -> Result<ExitToolCall, AskAgentError> {
                let now = self
                    .in_flight
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                self.max_observed
                    .fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                self.in_flight
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ExitToolCall {
                    name: "notify".into(),
                    args: serde_json::json!({}),
                })
            }
        }
        let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_observed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inv: Arc<dyn AgentInvoker> = Arc::new(DelayedStub {
            in_flight,
            max_observed: max_observed.clone(),
        });
        let pool = AutomationsAgentPool::with_concurrency(inv, 2);
        let req = AskAgentRequest::from_config(cfg_no_attachments());
        let p1 = pool.clone();
        let r1 = req.clone();
        let p2 = pool.clone();
        let r2 = req.clone();
        let p3 = pool.clone();
        let r3 = req.clone();
        let _ = tokio::join!(
            tokio::spawn(async move { p1.invoke(&r1).await }),
            tokio::spawn(async move { p2.invoke(&r2).await }),
            tokio::spawn(async move { p3.invoke(&r3).await }),
        );
        let m = max_observed.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            m >= 2 && m <= 2,
            "pool=2 should peak at 2 concurrent, got {m}"
        );
    }

    #[test]
    fn model_id_vision_heuristic_detects_known_vision_models() {
        for id in [
            "Qwen/Qwen2.5-VL-32B-Instruct",
            "Qwen/Qwen3-VL-7B",
            "mistralai/Pixtral-12B-2409",
            "meta-llama/Llama-3.2-11B-Vision-Instruct",
            "llava-hf/llava-onevision-7b",
            "OpenGVLab/InternVL2-8B",
            "microsoft/Phi-3.5-vision-instruct",
        ] {
            assert!(
                model_id_is_vision_capable(id),
                "{id} should be detected as vision-capable",
            );
        }
    }

    #[test]
    fn model_id_vision_heuristic_rejects_text_only_models() {
        for id in [
            "QuantTrio/Qwen3.5-27B-AWQ", // current default — text only
            "meta-llama/Llama-3.1-8B-Instruct",
            "mistralai/Mistral-7B-Instruct-v0.3",
            "google/gemma-2-9b-it",
            "anthropic/claude-3.5-sonnet",
        ] {
            assert!(
                !model_id_is_vision_capable(id),
                "{id} should be detected as text-only",
            );
        }
    }

    #[tokio::test]
    async fn inference_invoker_returns_no_llm_when_resolver_returns_none() {
        let db = test_db();
        let resolver = Arc::new(InferenceResolver::new(None));
        let invoker = InferenceAgentInvoker::new(db, resolver);
        let err = invoker
            .invoke(&AskAgentRequest::from_config(cfg_no_attachments()))
            .await
            .unwrap_err();
        assert!(matches!(err, AskAgentError::NoLlmConfigured));
    }

    /// M5: when a Vision backend row is configured AND attachments
    /// are present, the invoker routes to it directly — no heuristic
    /// fallback, no fail-fast on the Standard model's name. The
    /// operator's configuration is the source of truth.
    #[tokio::test]
    async fn inference_invoker_routes_to_vision_backend_when_attachments_present() {
        use execlaw_core::backends::{BackendMode, BackendPurpose, BackendStore, BackendUpsert};
        let db = test_db();
        // Seed a Vision row pointing at a bogus URL (the invoker
        // routes there but the actual HTTP call will fail — that's
        // fine, this test verifies routing, not transport). The URL
        // failure surfaces as `LlmFailure`, not
        // `VisionRequiredButTextOnlyModel` — proving the vision row
        // was selected.
        BackendStore::new(&db)
            .upsert(
                &BackendUpsert {
                    purpose: BackendPurpose::Vision,
                    inference_backend: "external".into(),
                    model_spec_json: serde_json::json!({
                        "args": ["--model=Qwen/Qwen2.5-VL-7B-Instruct"]
                    }),
                    gpu_id: None,
                    endpoint: Some("http://127.0.0.1:1".into()),
                    notes: None,
                    reasoning_enabled: false,
                    mode: BackendMode::External,
                },
                chrono::Utc::now().timestamp(),
            )
            .unwrap();
        let resolver = Arc::new(InferenceResolver::new(None));
        let invoker = InferenceAgentInvoker::new(db, resolver);
        let err = invoker
            .invoke(&AskAgentRequest::from_config(cfg_with_image()))
            .await
            .unwrap_err();
        // Vision row WAS selected → no VisionRequiredButTextOnlyModel.
        // The bogus URL produces an LlmFailure.
        match err {
            AskAgentError::LlmFailure(_) => {}
            AskAgentError::VisionRequiredButTextOnlyModel { .. } => panic!(
                "vision row was configured — invoker should not fail with VisionRequiredButTextOnlyModel",
            ),
            other => panic!("expected LlmFailure (bogus URL), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn inference_invoker_rejects_vision_on_text_only_model() {
        // Force the bootstrap resolver to return a text-only model
        // by giving it a client + a model id that won't match the
        // vision heuristic. The chat request never goes out because
        // the capability check fires first — so the URL can be a
        // bogus loopback.
        let db = test_db();
        let bootstrap = Arc::new(InferenceClient::new("http://127.0.0.1:1"));
        let mut resolver = InferenceResolver::new(Some(bootstrap));
        resolver.bootstrap_model = Some("QuantTrio/Qwen3.5-27B-AWQ".into());
        let invoker = InferenceAgentInvoker::new(db, Arc::new(resolver));
        let err = invoker
            .invoke(&AskAgentRequest::from_config(cfg_with_image()))
            .await
            .unwrap_err();
        match err {
            AskAgentError::VisionRequiredButTextOnlyModel { model_id } => {
                assert_eq!(model_id, "QuantTrio/Qwen3.5-27B-AWQ");
            }
            other => panic!("expected VisionRequiredButTextOnlyModel, got {other:?}"),
        }
    }

    fn test_db() -> Database {
        use execlaw_core::db::DbConfig;
        use execlaw_core::migrations::MigrationRunner;
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }
}
