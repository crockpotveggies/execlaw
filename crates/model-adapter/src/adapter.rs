//! [`ModelAdapter`] trait + the typed inputs/outputs every adapter
//! impl shares.

use crate::extract;
use crate::family::ModelFamily;
use async_trait::async_trait;
use execlaw_inference_api::{ChatRequest, ChatResponse, InferenceClient, InferenceError, Usage};

/// Hint the caller passes so the adapter can apply the most
/// appropriate transformation. The adapter doesn't FORCE any
/// behavior based on this; it just informs prep + post-processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputHint {
    /// Caller will parse the content as JSON. Adapter suppresses
    /// reasoning, strips code fences, prefers low temperature.
    StructuredJson,
    /// Caller wants markdown. Adapter suppresses reasoning, leaves
    /// fences alone (markdown often contains code blocks).
    Markdown,
    /// Caller wants conversation reply (used by chat handler +
    /// title gen). Reasoning suppression is honored when the
    /// caller's `chat_template_kwargs` doesn't already opt into
    /// thinking; otherwise we pass through.
    Conversation,
    /// Plain text body, adapter does minimal normalization
    /// (reasoning extraction only; no fence/preamble stripping
    /// because callers may want the verbatim model output).
    Plain,
}

/// What the adapter returns after post-processing. `content` is the
/// caller-visible text after reasoning blocks + fences + preambles
/// are stripped; `reasoning` is whatever was extracted (returned for
/// telemetry / future reasoning-aware UI surfaces).
#[derive(Debug, Clone)]
pub struct AdaptedResponse {
    pub content: String,
    pub reasoning: Option<String>,
    /// Stable string copied from the upstream `ChatResponse` so the
    /// caller can branch on `"stop"` / `"length"` / `"tool_calls"`.
    pub finish_reason: Option<String>,
    /// Raw OpenAI-shaped tool calls, untouched. Tool-call format
    /// normalization (Llama `<|python_tag|>`, R1 Python-style) is
    /// a documented v2 gap — see lib.rs.
    pub tool_calls: Vec<execlaw_inference_api::ToolCall>,
    /// Model id the upstream actually used (vLLM may rewrite the
    /// requested model_id in the response). Useful for telemetry.
    pub model_id: String,
    /// Token-usage block from the upstream response. Useful for
    /// telemetry + cost accounting; preserved verbatim so callers
    /// don't have to re-do the chat_completions call to get it.
    pub usage: Option<Usage>,
}

/// The trait every family impl satisfies. Default `chat()` method
/// composes prep + call + process so call sites stay one-liners.
#[async_trait]
pub trait ModelAdapter: Send + Sync {
    fn family(&self) -> ModelFamily;

    /// Apply per-family request preparation. Pure: no I/O. The
    /// caller hands in a base `ChatRequest`; the adapter returns a
    /// (possibly modified) request to send to the inference client.
    fn prepare_request(&self, req: ChatRequest, hint: OutputHint) -> ChatRequest;

    /// Post-process a non-streaming response. Pure: no I/O.
    /// Extracts reasoning, normalizes content, copies finish_reason
    /// and tool_calls verbatim.
    fn process_response(&self, resp: ChatResponse, hint: OutputHint) -> AdaptedResponse;

    /// Convenience: prepare → call → process. The default impl is
    /// what every call site should use.
    async fn chat(
        &self,
        client: &InferenceClient,
        req: ChatRequest,
        hint: OutputHint,
    ) -> Result<AdaptedResponse, InferenceError> {
        let prepped = self.prepare_request(req, hint);
        let resp = client.chat_completions(&prepped).await?;
        Ok(self.process_response(resp, hint))
    }
}

// -----------------------------------------------------------------
// Shared post-processing helpers used by multiple family impls.
// Kept here (not in `extract.rs`) because they construct the typed
// `AdaptedResponse` rather than just returning strings.
// -----------------------------------------------------------------

/// Pull the first choice's text + finish_reason + tool_calls from a
/// vLLM/OpenAI-shaped response, dropping the rest.
pub(crate) fn first_choice(resp: &ChatResponse) -> (String, Option<String>, Vec<execlaw_inference_api::ToolCall>) {
    let model_id = resp.model.clone();
    let _ = model_id;
    if let Some(choice) = resp.choices.first() {
        let text = choice.message.content.clone().unwrap_or_default();
        let finish = choice.finish_reason.clone();
        let tools = choice.message.tool_calls.clone();
        (text, finish, tools)
    } else {
        (String::new(), None, Vec::new())
    }
}

/// Apply the standard "strip preamble + fences" pass for hints
/// where the caller wants clean structured / plain text. Returns
/// `(reasoning, content)`.
pub(crate) fn standard_normalize(text: &str, hint: OutputHint) -> (Option<String>, String) {
    let (reasoning, after_think) = extract::split_think_block(text);
    let after_preamble = extract::strip_thinking_process_preamble(&after_think);
    let final_content = match hint {
        OutputHint::StructuredJson | OutputHint::Plain => {
            // For JSON the caller (e.g. planner) does its own
            // brace walk; we just strip a fence pair if present.
            extract::strip_code_fences(&after_preamble)
        }
        OutputHint::Markdown => {
            // Don't strip fences from markdown — the body often
            // legitimately contains them.
            after_preamble
        }
        OutputHint::Conversation => {
            // Conversational text: leave alone, the chat handler
            // shows it verbatim. (Streaming uses ThinkBlockFilter.)
            after_preamble
        }
    };
    (reasoning, final_content)
}
