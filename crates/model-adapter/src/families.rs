//! One [`ModelAdapter`] impl per family. Each impl is small (its
//! per-family quirks are isolated to its `prepare_request` and
//! `process_response` overrides).
//!
//! The selection helper [`adapter_for`] returns a boxed adapter
//! given a `ModelFamily`; call sites typically write:
//!
//! ```ignore
//! let adapter = execlaw_model_adapter::adapter_for(
//!     ModelFamily::detect(&model_id),
//! );
//! let adapted = adapter.chat(&client, req, OutputHint::StructuredJson).await?;
//! ```

use crate::adapter::{first_choice, standard_normalize, AdaptedResponse, ModelAdapter, OutputHint};
use crate::extract;
use crate::family::ModelFamily;
use async_trait::async_trait;
use execlaw_inference_api::{ChatMessage, ChatRequest, ChatResponse, Role};
use serde_json::json;

/// Return a boxed adapter for the given family. Cheap construction
/// (no allocation beyond the Box itself); call sites usually build
/// one per request.
pub fn adapter_for(family: ModelFamily) -> Box<dyn ModelAdapter> {
    match family {
        ModelFamily::Qwen3 => Box::new(Qwen3Adapter),
        ModelFamily::DeepSeekR1 => Box::new(DeepSeekR1Adapter),
        ModelFamily::DeepSeekV3 => Box::new(DeepSeekV3Adapter),
        ModelFamily::Llama3 => Box::new(Llama3Adapter),
        ModelFamily::Mistral => Box::new(MistralAdapter),
        ModelFamily::Gemma => Box::new(GemmaAdapter),
        ModelFamily::OpenAiGeneric => Box::new(OpenAiGenericAdapter),
    }
}

// ---------------- Qwen3 ----------------

pub struct Qwen3Adapter;

#[async_trait]
impl ModelAdapter for Qwen3Adapter {
    fn family(&self) -> ModelFamily {
        ModelFamily::Qwen3
    }

    fn prepare_request(&self, mut req: ChatRequest, hint: OutputHint) -> ChatRequest {
        // Qwen3.5's chat template honors `enable_thinking`. Default
        // OFF for everything except reasoning-capable conversation
        // (none today). Don't clobber a caller-set kwarg —
        // Conversation hint explicitly preserves whatever the chat
        // handler chose (it reads `reasoning_enabled` from
        // `config_backends`).
        match hint {
            OutputHint::Conversation => {
                if req.chat_template_kwargs.is_none() {
                    req.chat_template_kwargs = Some(json!({"enable_thinking": false}));
                }
            }
            _ => {
                req.chat_template_kwargs = Some(json!({"enable_thinking": false}));
            }
        }
        req
    }

    fn process_response(&self, resp: ChatResponse, hint: OutputHint) -> AdaptedResponse {
        let (text, finish, tool_calls) = first_choice(&resp);
        let (reasoning, content) = standard_normalize(&text, hint);
        AdaptedResponse {
            content,
            reasoning,
            finish_reason: finish,
            tool_calls,
            model_id: resp.model,
            usage: resp.usage,
        }
    }
}

// ---------------- DeepSeek R1 ----------------

pub struct DeepSeekR1Adapter;

#[async_trait]
impl ModelAdapter for DeepSeekR1Adapter {
    fn family(&self) -> ModelFamily {
        ModelFamily::DeepSeekR1
    }

    fn prepare_request(&self, req: ChatRequest, _hint: OutputHint) -> ChatRequest {
        // R1 does not honor `enable_thinking`. We strip reasoning
        // post-hoc instead. Pass through verbatim.
        req
    }

    fn process_response(&self, resp: ChatResponse, hint: OutputHint) -> AdaptedResponse {
        let (text, finish, tool_calls) = first_choice(&resp);
        // R1 emits `<think>...</think>` blocks heavily.
        let (reasoning, content) = standard_normalize(&text, hint);
        AdaptedResponse {
            content,
            reasoning,
            finish_reason: finish,
            tool_calls,
            model_id: resp.model,
            usage: resp.usage,
        }
    }
}

// ---------------- DeepSeek V3 ----------------

pub struct DeepSeekV3Adapter;

#[async_trait]
impl ModelAdapter for DeepSeekV3Adapter {
    fn family(&self) -> ModelFamily {
        ModelFamily::DeepSeekV3
    }

    fn prepare_request(&self, req: ChatRequest, _hint: OutputHint) -> ChatRequest {
        req
    }

    fn process_response(&self, resp: ChatResponse, hint: OutputHint) -> AdaptedResponse {
        let (text, finish, tool_calls) = first_choice(&resp);
        let (reasoning, content) = standard_normalize(&text, hint);
        AdaptedResponse {
            content,
            reasoning,
            finish_reason: finish,
            tool_calls,
            model_id: resp.model,
            usage: resp.usage,
        }
    }
}

// ---------------- Llama 3 ----------------

pub struct Llama3Adapter;

#[async_trait]
impl ModelAdapter for Llama3Adapter {
    fn family(&self) -> ModelFamily {
        ModelFamily::Llama3
    }

    fn prepare_request(&self, req: ChatRequest, _hint: OutputHint) -> ChatRequest {
        // Llama doesn't think; no kwargs needed. Tool-call
        // `<|python_tag|>` shape lands in the text body — v2 will
        // surface those into `tool_calls[]` from here. For now,
        // pass through.
        req
    }

    fn process_response(&self, resp: ChatResponse, hint: OutputHint) -> AdaptedResponse {
        let (text, finish, tool_calls) = first_choice(&resp);
        // Llama doesn't emit <think> but may wrap structured output
        // in fences spontaneously. Use standard_normalize (strips
        // fences for JSON hint, leaves alone for Markdown).
        let (_unused_reasoning, content) = standard_normalize(&text, hint);
        AdaptedResponse {
            content,
            reasoning: None,
            finish_reason: finish,
            tool_calls,
            model_id: resp.model,
            usage: resp.usage,
        }
    }
}

// ---------------- Mistral / Mixtral ----------------

pub struct MistralAdapter;

#[async_trait]
impl ModelAdapter for MistralAdapter {
    fn family(&self) -> ModelFamily {
        ModelFamily::Mistral
    }

    fn prepare_request(&self, req: ChatRequest, _hint: OutputHint) -> ChatRequest {
        req
    }

    fn process_response(&self, resp: ChatResponse, hint: OutputHint) -> AdaptedResponse {
        let (text, finish, tool_calls) = first_choice(&resp);
        let (_, content) = standard_normalize(&text, hint);
        AdaptedResponse {
            content,
            reasoning: None,
            finish_reason: finish,
            tool_calls,
            model_id: resp.model,
            usage: resp.usage,
        }
    }
}

// ---------------- Gemma ----------------

pub struct GemmaAdapter;

#[async_trait]
impl ModelAdapter for GemmaAdapter {
    fn family(&self) -> ModelFamily {
        ModelFamily::Gemma
    }

    fn prepare_request(&self, mut req: ChatRequest, _hint: OutputHint) -> ChatRequest {
        // Gemma's chat template rejects the `system` role outright.
        // Merge any leading system messages into the first user
        // message (the standard workaround across vLLM Gemma users).
        // No-op if no system message is present.
        req.messages = merge_system_into_user(req.messages);
        req
    }

    fn process_response(&self, resp: ChatResponse, hint: OutputHint) -> AdaptedResponse {
        let (text, finish, tool_calls) = first_choice(&resp);
        let (_, content) = standard_normalize(&text, hint);
        AdaptedResponse {
            content,
            reasoning: None,
            finish_reason: finish,
            tool_calls,
            model_id: resp.model,
            usage: resp.usage,
        }
    }
}

fn merge_system_into_user(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    if messages.is_empty() {
        return messages;
    }
    // Collect all leading system messages, then prepend their
    // joined content into the first user message.
    let mut system_parts: Vec<String> = Vec::new();
    let mut iter = messages.into_iter().peekable();
    while let Some(m) = iter.peek() {
        if matches!(m.role, Role::System) {
            let m = iter.next().unwrap();
            if let Some(c) = m.content {
                if !c.is_empty() {
                    system_parts.push(c);
                }
            }
        } else {
            break;
        }
    }
    let mut out: Vec<ChatMessage> = iter.collect();
    if system_parts.is_empty() {
        return out;
    }
    let merged = system_parts.join("\n\n");
    // Find the first user message and prepend; if there is none,
    // create one (defensive — Gemma needs a user turn).
    if let Some(first_user) = out.iter_mut().find(|m| matches!(m.role, Role::User)) {
        let new_content = match first_user.content.take() {
            Some(orig) if !orig.is_empty() => format!("{merged}\n\n{orig}"),
            _ => merged,
        };
        first_user.content = Some(new_content);
    } else {
        out.insert(0, ChatMessage::user(merged));
    }
    out
}

// ---------------- OpenAI generic ----------------

pub struct OpenAiGenericAdapter;

#[async_trait]
impl ModelAdapter for OpenAiGenericAdapter {
    fn family(&self) -> ModelFamily {
        ModelFamily::OpenAiGeneric
    }

    fn prepare_request(&self, req: ChatRequest, _hint: OutputHint) -> ChatRequest {
        // Safest: pass through. Don't add kwargs an unknown backend
        // might not understand.
        req
    }

    fn process_response(&self, resp: ChatResponse, hint: OutputHint) -> AdaptedResponse {
        let (text, finish, tool_calls) = first_choice(&resp);
        // Safest stripping: a <think> block if it happens to be
        // there (cheap to check), and fences for structured hints.
        let (reasoning, mut content) = extract::split_think_block(&text);
        if matches!(hint, OutputHint::StructuredJson | OutputHint::Plain) {
            content = extract::strip_code_fences(&content);
        }
        AdaptedResponse {
            content,
            reasoning,
            finish_reason: finish,
            tool_calls,
            model_id: resp.model,
            usage: resp.usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_inference_api::{ChatMessage, Choice, ChatResponse, ModelId, Role};

    fn resp_with_content(text: &str) -> ChatResponse {
        ChatResponse {
            id: "1".into(),
            model: "test-model".into(),
            choices: vec![Choice {
                index: 0,
                message: ChatMessage {
                    role: Role::Assistant,
                    content: Some(text.into()),
                    tool_call_id: None,
                    name: None,
                    tool_calls: vec![],
                },
                finish_reason: Some("stop".into()),
            }],
            usage: None,
        }
    }

    fn req() -> ChatRequest {
        ChatRequest {
            model: ModelId("test".into()),
            messages: vec![
                ChatMessage::system("sys"),
                ChatMessage::user("hi"),
            ],
            tools: None,
            stream: false,
            temperature: None,
            max_tokens: None,
            chat_template_kwargs: None,
        }
    }

    // --- Qwen3 ---

    #[test]
    fn qwen3_prepare_sets_enable_thinking_false_for_structured() {
        let r = Qwen3Adapter.prepare_request(req(), OutputHint::StructuredJson);
        assert_eq!(
            r.chat_template_kwargs.unwrap(),
            json!({"enable_thinking": false})
        );
    }

    #[test]
    fn qwen3_prepare_preserves_caller_kwargs_for_conversation() {
        let mut base = req();
        base.chat_template_kwargs = Some(json!({"enable_thinking": true}));
        let r = Qwen3Adapter.prepare_request(base, OutputHint::Conversation);
        // Caller's choice wins for Conversation.
        assert_eq!(
            r.chat_template_kwargs.unwrap(),
            json!({"enable_thinking": true})
        );
    }

    #[test]
    fn qwen3_process_strips_think_block_and_fences() {
        let r = Qwen3Adapter.process_response(
            resp_with_content("<think>weighing options</think>```json\n{\"x\":1}\n```"),
            OutputHint::StructuredJson,
        );
        assert_eq!(r.content, "{\"x\":1}");
        assert_eq!(r.reasoning.as_deref(), Some("weighing options"));
    }

    #[test]
    fn qwen3_process_strips_thinking_process_preamble() {
        let r = Qwen3Adapter.process_response(
            resp_with_content("Thinking Process: foo bar\n\n{\"x\":1}"),
            OutputHint::StructuredJson,
        );
        assert_eq!(r.content, "{\"x\":1}");
    }

    // --- DeepSeek R1 ---

    #[test]
    fn deepseek_r1_strips_reasoning_block() {
        let r = DeepSeekR1Adapter.process_response(
            resp_with_content("<think>step 1\nstep 2</think>final answer"),
            OutputHint::Markdown,
        );
        assert_eq!(r.content, "final answer");
        assert!(r.reasoning.unwrap().contains("step 2"));
    }

    #[test]
    fn deepseek_r1_does_not_set_chat_template_kwargs() {
        let r = DeepSeekR1Adapter.prepare_request(req(), OutputHint::StructuredJson);
        assert!(r.chat_template_kwargs.is_none());
    }

    // --- Llama 3 ---

    #[test]
    fn llama3_strips_fences_for_structured_hint() {
        let r = Llama3Adapter.process_response(
            resp_with_content("```json\n{\"a\":1}\n```"),
            OutputHint::StructuredJson,
        );
        assert_eq!(r.content, "{\"a\":1}");
    }

    #[test]
    fn llama3_passes_markdown_through_unchanged() {
        let r = Llama3Adapter.process_response(
            resp_with_content("# Title\n\nbody with `code`"),
            OutputHint::Markdown,
        );
        assert!(r.content.contains("# Title"));
        assert!(r.content.contains("`code`"));
    }

    // --- Mistral ---

    #[test]
    fn mistral_strips_fences_for_structured_hint() {
        let r = MistralAdapter.process_response(
            resp_with_content("```\n{\"k\":\"v\"}\n```"),
            OutputHint::StructuredJson,
        );
        assert_eq!(r.content, "{\"k\":\"v\"}");
    }

    // --- Gemma ---

    #[test]
    fn gemma_merges_leading_system_into_first_user() {
        let r = GemmaAdapter.prepare_request(req(), OutputHint::Conversation);
        assert_eq!(r.messages.len(), 1);
        assert!(matches!(r.messages[0].role, Role::User));
        let body = r.messages[0].content.as_deref().unwrap();
        assert!(body.contains("sys"));
        assert!(body.contains("hi"));
    }

    #[test]
    fn gemma_handles_multiple_system_messages() {
        let mut base = req();
        base.messages = vec![
            ChatMessage::system("rule 1"),
            ChatMessage::system("rule 2"),
            ChatMessage::user("question"),
        ];
        let r = GemmaAdapter.prepare_request(base, OutputHint::Conversation);
        assert_eq!(r.messages.len(), 1);
        let body = r.messages[0].content.as_deref().unwrap();
        assert!(body.contains("rule 1"));
        assert!(body.contains("rule 2"));
        assert!(body.contains("question"));
    }

    #[test]
    fn gemma_creates_user_turn_when_only_system_present() {
        // Defensive — should never happen, but the merger handles it.
        let mut base = req();
        base.messages = vec![ChatMessage::system("orphan")];
        let r = GemmaAdapter.prepare_request(base, OutputHint::Conversation);
        assert_eq!(r.messages.len(), 1);
        assert!(matches!(r.messages[0].role, Role::User));
    }

    // --- OpenAI generic ---

    #[test]
    fn generic_passes_request_through_unchanged() {
        let r = OpenAiGenericAdapter.prepare_request(req(), OutputHint::StructuredJson);
        assert!(r.chat_template_kwargs.is_none());
        assert_eq!(r.messages.len(), 2);
    }

    #[test]
    fn generic_strips_think_block_defensively() {
        // Even unknown backends sometimes route through a model
        // that leaks <think>; cheap to handle.
        let r = OpenAiGenericAdapter.process_response(
            resp_with_content("<think>x</think>visible"),
            OutputHint::Plain,
        );
        assert_eq!(r.content, "visible");
        assert_eq!(r.reasoning.as_deref(), Some("x"));
    }

    // --- factory ---

    #[test]
    fn adapter_for_returns_correct_family() {
        for fam in [
            ModelFamily::Qwen3,
            ModelFamily::DeepSeekR1,
            ModelFamily::DeepSeekV3,
            ModelFamily::Llama3,
            ModelFamily::Mistral,
            ModelFamily::Gemma,
            ModelFamily::OpenAiGeneric,
        ] {
            assert_eq!(adapter_for(fam).family(), fam);
        }
    }

    // --- finish_reason + tool_calls passthrough ---

    #[test]
    fn finish_reason_and_tool_calls_pass_through_verbatim() {
        let mut resp = resp_with_content("ok");
        resp.choices[0].finish_reason = Some("tool_calls".into());
        resp.choices[0].message.tool_calls = vec![execlaw_inference_api::ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: execlaw_inference_api::ToolCallFunction {
                name: "search".into(),
                arguments: "{\"q\":\"x\"}".into(),
            },
        }];
        let r = Qwen3Adapter.process_response(resp, OutputHint::Conversation);
        assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].function.name, "search");
    }
}
