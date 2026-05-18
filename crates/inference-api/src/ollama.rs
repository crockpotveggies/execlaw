//! Ollama-native chat path.
//!
//! Ollama exposes two HTTP surfaces:
//!
//!   * `/v1/chat/completions` — the OpenAI-compat shim. Same wire
//!     shape as vLLM / llama-server / OpenArc, so everything else in
//!     `inference-api` targets it.
//!   * `/api/chat`            — Ollama's native endpoint. Same
//!     conceptual fields (messages, tools), but the response carries
//!     `tool_calls` reliably even on small models where the OpenAI
//!     shim drops them.
//!
//! The shim's tool-call extraction has been observed to silently
//! return plain `content` text for `qwen2.5:3b-instruct-q4_K_M` even
//! when the model emits a valid Hermes-style tool call — the agent
//! then renders `(web_search "…")` literal in the chat UI. The
//! native endpoint returns the same `tool_calls` array on the same
//! prompt, every time. Until upstream Ollama fixes the shim, the
//! supervisor routes Apple-Silicon Ollama backends here.
//!
//! What this module does NOT do:
//!
//!   * It doesn't replace the OpenAI path for vLLM / llama-server.
//!     Those engines have first-class tool support through the shim
//!     and their own structured-output knobs (`guided_decoding_*`).
//!     `inference-api`'s caller selects this path via
//!     `InferenceClient::with_engine(InferenceEngine::Ollama)`.
//!   * It doesn't speak Ollama's `/api/generate` endpoint. Tool
//!     calling lives on `/api/chat`; `/api/generate` is the raw-prompt
//!     surface for non-conversational completions and isn't used by
//!     the agent.

use crate::{
    ChatMessage, ChatRequest, ChatResponse, ChatStreamChoice, ChatStreamChunk, ChatStreamDelta,
    Choice, InferenceError, ModelId, Role, ToolCall, ToolCallDelta, ToolCallFunction,
    ToolCallFunctionDelta, ToolDeclaration, Usage,
};
use serde::{Deserialize, Serialize};

/// Strip the trailing `/v1` (the OpenAI-compat suffix) from a base
/// URL so the native endpoints can be addressed on the daemon root.
/// `inference-api` callers hand us URLs like
/// `http://127.0.0.1:8101/v1`; Ollama's native API lives at
/// `http://127.0.0.1:8101/api/chat`.
///
/// Idempotent for URLs that don't carry the suffix — operators who
/// manually configured a bare `http://host:port` still hit the right
/// path. Trailing slashes are trimmed before the `/v1` check so
/// `…:8101/v1/` is handled the same as `…:8101/v1`.
fn daemon_root(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .unwrap_or(trimmed)
        .to_owned()
}

// ---------------------------------------------------------------------------
// Wire types — Ollama's request / response shapes.
// ---------------------------------------------------------------------------

/// Ollama's `/api/chat` request body. Field set is intentionally
/// narrow: only what the agent's `ChatRequest` carries today. Fields
/// the OpenAI-compat side ships that Ollama silently ignores
/// (`tool_choice`, `guided_decoding_backend`, `chat_template_kwargs`)
/// are NOT forwarded — Ollama has no equivalent knobs.
#[derive(Debug, Serialize)]
struct OllamaChatRequest<'a> {
    model: &'a str,
    messages: Vec<OllamaMessage<'a>>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tools: &'a [ToolDeclaration],
    stream: bool,
    #[serde(skip_serializing_if = "OllamaOptions::is_empty")]
    options: OllamaOptions,
}

#[derive(Debug, Default, Serialize)]
struct OllamaOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    /// Ollama uses `num_predict` for what OpenAI calls `max_tokens`.
    /// Default in Ollama is 128; the agent's typical request asks
    /// for much more, so forwarding is required to get a useful cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
    /// Context window size. Ollama's default of 4096 is well below
    /// what the agent needs — a typical tool-bearing turn has a
    /// system prompt (~3 KB) + 36 tool schemas (~3 KB) + history,
    /// and lands around 6-8 K tokens. With `num_ctx=4096` Ollama
    /// silently truncates the middle (`keep=4 new=4096`), which
    /// drops the tool schemas while keeping recent tool_result
    /// payloads — the model knows the tool name from chat-template
    /// scaffolding but no longer has the schema or earlier results
    /// in view, so it re-queries indefinitely. 16384 covers every
    /// realistic agent turn for the Apple-Silicon Qwen2.5 family
    /// (3B/14B/32B all support ≥32K natively) at a few hundred MB
    /// of extra KV-cache memory.
    #[serde(skip_serializing_if = "Option::is_none")]
    num_ctx: Option<u32>,
}

impl OllamaOptions {
    fn is_empty(&self) -> bool {
        self.temperature.is_none() && self.num_predict.is_none() && self.num_ctx.is_none()
    }
}

/// Conservative context-window size for the Apple-Silicon Ollama
/// path. Big enough to fit the agent's system prompt + full tool
/// catalog + several rounds of tool results; small enough that the
/// KV cache stays in a few hundred MB. Qwen2.5 supports up to 32K
/// natively, but most agent turns finish well under 12K — 16K is
/// the sweet spot.
const DEFAULT_NUM_CTX: u32 = 16384;

/// Ollama's message shape on the wire — close enough to OpenAI's
/// that we serialize a borrowed view rather than cloning. `name`
/// (used by OpenAI for tool-result attribution) is dropped; Ollama
/// uses `tool_call_id` correlation only.
#[derive(Debug, Serialize)]
struct OllamaMessage<'a> {
    role: &'a str,
    /// Ollama accepts plain string content; the multipart image
    /// representation lives on a separate `images` array (not used
    /// by the v1 Apple-Silicon path — Qwen2.5-instruct quants are
    /// text-only). We render `MessageContent::Text` here; if the
    /// upstream caller ever sends `Parts` we flatten back to the
    /// joined text and drop the image parts with a warning.
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
    /// Ollama's request-side tool_calls array; only meaningful on a
    /// re-feed of an assistant turn that previously emitted tool
    /// calls. Same wire shape as OpenAI's.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OllamaToolCall>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    function: OllamaFunctionCall,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaFunctionCall {
    name: String,
    /// Ollama emits `arguments` as a JSON OBJECT (e.g.
    /// `{"query":"weather"}`). The OpenAI spec wants it as a JSON
    /// STRING (e.g. `"{\"query\":\"weather\"}"`). We carry the raw
    /// `Value` here and stringify at the boundary.
    arguments: serde_json::Value,
}

/// Non-streaming `/api/chat` response. `eval_count` /
/// `prompt_eval_count` map cleanly to OpenAI's
/// `usage.{completion_tokens, prompt_tokens}`.
#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    model: String,
    #[serde(default)]
    message: OllamaResponseMessage,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    eval_count: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct OllamaResponseMessage {
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<OllamaToolCall>,
}

/// One frame from `/api/chat?stream=true` — Ollama emits NDJSON,
/// one JSON object per `\n`-terminated line. `done: true` marks the
/// terminal frame; tool calls typically only appear on that final
/// frame.
#[derive(Debug, Deserialize)]
struct OllamaStreamFrame {
    #[serde(default)]
    message: OllamaResponseMessage,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Request translation
// ---------------------------------------------------------------------------

/// Build the Ollama-native request body from the OpenAI-shaped
/// `ChatRequest`. `stream` is overridden explicitly by the caller —
/// the field on `ChatRequest` is advisory and changes per call site.
fn build_request<'a>(req: &'a ChatRequest, stream: bool) -> OllamaChatRequest<'a> {
    let messages = req.messages.iter().map(translate_message).collect();
    OllamaChatRequest {
        model: req.model.as_str(),
        messages,
        tools: req.tools.as_deref().unwrap_or(&[]),
        stream,
        options: OllamaOptions {
            temperature: req.temperature,
            num_predict: req.max_tokens,
            num_ctx: Some(DEFAULT_NUM_CTX),
        },
    }
}

fn translate_message(m: &ChatMessage) -> OllamaMessage<'_> {
    let role = match m.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let content = m.content.as_ref().map(content_to_plain);
    let tool_calls = m
        .tool_calls
        .iter()
        .map(|tc| OllamaToolCall {
            id: Some(tc.id.clone()),
            function: OllamaFunctionCall {
                name: tc.function.name.clone(),
                // OpenAI carries arguments as a JSON STRING; Ollama
                // wants the parsed VALUE. Parse on the way out; on
                // bad-JSON fall back to a string-wrapped object so
                // Ollama at least sees the raw text.
                arguments: serde_json::from_str(&tc.function.arguments).unwrap_or_else(|_| {
                    serde_json::Value::String(tc.function.arguments.clone())
                }),
            },
        })
        .collect();
    OllamaMessage {
        role,
        content,
        tool_call_id: m.tool_call_id.as_deref(),
        tool_calls,
    }
}

/// Best-effort flatten of `MessageContent::Parts` to a plain string.
/// Image parts get a short text placeholder so the model sees
/// "[image attached]" rather than silently nothing — Ollama's
/// text-only quants on Apple Silicon can't render images anyway.
fn content_to_plain(c: &crate::MessageContent) -> String {
    use crate::ContentPart;
    use crate::MessageContent;
    match c {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Parts(parts) => {
            let mut out = String::new();
            for p in parts {
                match p {
                    ContentPart::Text { text } => out.push_str(text),
                    ContentPart::ImageUrl { .. } => out.push_str("\n[image attached]\n"),
                }
            }
            out
        }
    }
}

// ---------------------------------------------------------------------------
// Response translation
// ---------------------------------------------------------------------------

/// Translate Ollama's `/api/chat` response into the OpenAI-shaped
/// `ChatResponse` the rest of execlaw consumes. The mapping is
/// total — every Ollama field with a meaningful equivalent is
/// surfaced; fields with no equivalent (e.g. `total_duration`) drop.
fn response_to_openai(
    raw: OllamaChatResponse,
    request_model: &ModelId,
) -> ChatResponse {
    let usage = if raw.prompt_eval_count.is_some() || raw.eval_count.is_some() {
        Some(Usage {
            prompt_tokens: raw.prompt_eval_count.unwrap_or(0),
            completion_tokens: raw.eval_count.unwrap_or(0),
            total_tokens: raw.prompt_eval_count.unwrap_or(0) + raw.eval_count.unwrap_or(0),
        })
    } else {
        None
    };

    let message = ChatMessage {
        role: match raw.message.role.as_str() {
            "system" => Role::System,
            "user" => Role::User,
            "tool" => Role::Tool,
            _ => Role::Assistant,
        },
        content: if raw.message.content.is_empty() {
            None
        } else {
            Some(crate::MessageContent::Text(raw.message.content))
        },
        tool_call_id: None,
        name: None,
        tool_calls: raw
            .message
            .tool_calls
            .into_iter()
            .enumerate()
            .map(|(idx, tc)| ToolCall {
                id: tc.id.unwrap_or_else(|| format!("call_{idx}")),
                kind: "function".into(),
                function: ToolCallFunction {
                    name: tc.function.name,
                    // OpenAI: arguments is a STRING. Stringify the
                    // raw JSON value here so downstream parsers
                    // that `serde_json::from_str(arguments)` work
                    // identically to the vLLM path.
                    arguments: tc.function.arguments.to_string(),
                },
            })
            .collect(),
    };

    ChatResponse {
        // Ollama doesn't mint a stable id per response; synthesize
        // one from the model name + a nanosecond timestamp so the
        // SPA's chat-event correlation still has a unique handle.
        id: format!(
            "ollama-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ),
        // Echo whatever model the caller asked for; the body's
        // `raw.model` is the same string but downstream code keys on
        // request.model in places, so keep them aligned.
        model: if raw.model.is_empty() {
            request_model.as_str().to_owned()
        } else {
            raw.model
        },
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason: raw.done_reason.or_else(|| Some("stop".to_owned())),
        }],
        usage,
    }
}

// ---------------------------------------------------------------------------
// Public client surface — invoked from InferenceClient when its
// engine is Ollama.
// ---------------------------------------------------------------------------

/// Non-streaming POST `/api/chat`. Mirrors the contract of
/// `InferenceClient::chat_completions` so the engine switch is
/// transparent to callers.
pub(crate) async fn chat_completions(
    http: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    req: &ChatRequest,
) -> Result<ChatResponse, InferenceError> {
    let url = format!("{}/api/chat", daemon_root(base_url));
    let body = build_request(req, false);
    let mut r = http.post(&url).json(&body);
    if let Some(key) = api_key {
        r = r.bearer_auth(key);
    }
    let resp = r.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(InferenceError::BadStatus {
            status: status.as_u16(),
            body,
        });
    }
    let text = resp.text().await?;
    let raw: OllamaChatResponse = serde_json::from_str(&text).map_err(|e| {
        InferenceError::Decode(format!(
            "bad /api/chat response: {e} — body: {text}"
        ))
    })?;
    Ok(response_to_openai(raw, &req.model))
}

/// Streaming POST `/api/chat`. Ollama emits NDJSON (one JSON object
/// per line). We translate each frame into a `ChatStreamChunk` so
/// the rest of execlaw can stay on its OpenAI-flavored stream
/// consumer.
pub(crate) async fn chat_completions_stream(
    http: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    req: &ChatRequest,
) -> Result<
    std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<ChatStreamChunk, InferenceError>> + Send>,
    >,
    InferenceError,
> {
    use futures::StreamExt;

    let url = format!("{}/api/chat", daemon_root(base_url));
    let body = build_request(req, true);
    let mut r = http.post(&url).json(&body);
    if let Some(key) = api_key {
        r = r.bearer_auth(key);
    }
    let resp = r.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(InferenceError::BadStatus {
            status: status.as_u16(),
            body,
        });
    }

    // Synthesize a stable id + model echo so the chunk-aggregator
    // upstream doesn't have to handle "no id yet".
    let stream_id = format!(
        "ollama-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let model_echo = req.model.as_str().to_owned();

    let bytes_stream = resp.bytes_stream();
    // Carry a rolling buffer so a chunk that splits a line across
    // two TCP reads still parses. Wrap the state in a struct that
    // implements Stream<Item = Result<ChatStreamChunk, _>>.
    let chunks = ndjson_to_chat_chunks(bytes_stream, stream_id, model_echo);
    Ok(Box::pin(chunks))
}

/// NDJSON → ChatStreamChunk adapter. Split out for unit testing
/// against a synthetic byte stream without standing up an HTTP
/// server.
fn ndjson_to_chat_chunks<S>(
    bytes_stream: S,
    stream_id: String,
    model: String,
) -> impl futures::Stream<Item = Result<ChatStreamChunk, InferenceError>> + Send
where
    S: futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
{
    use futures::StreamExt;
    let state = NdjsonState::default();
    futures::stream::unfold(
        (Box::pin(bytes_stream), state, stream_id, model, false),
        |(mut s, mut state, id, model, mut emitted_role)| async move {
            loop {
                // Try to emit a chunk from the buffer first.
                if let Some(line) = state.next_line() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<OllamaStreamFrame>(trimmed) {
                        Ok(frame) => {
                            let chunk = frame_to_chunk(
                                frame,
                                &id,
                                &model,
                                &mut emitted_role,
                            );
                            return Some((
                                Ok(chunk),
                                (s, state, id, model, emitted_role),
                            ));
                        }
                        Err(e) => {
                            return Some((
                                Err(InferenceError::Decode(format!(
                                    "bad /api/chat NDJSON frame: {e} — line: {trimmed}"
                                ))),
                                (s, state, id, model, emitted_role),
                            ));
                        }
                    }
                }
                // Buffer ran dry — pull more bytes.
                match s.next().await {
                    Some(Ok(bytes)) => state.extend(&bytes),
                    Some(Err(e)) => {
                        return Some((
                            Err(InferenceError::Http(e)),
                            (s, state, id, model, emitted_role),
                        ));
                    }
                    None => {
                        // EOF — drain any final partial line if it
                        // happens to be a complete JSON object
                        // without trailing newline (Ollama always
                        // sends one, but be lenient).
                        if let Some(line) = state.flush_tail() {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                if let Ok(frame) =
                                    serde_json::from_str::<OllamaStreamFrame>(trimmed)
                                {
                                    let chunk = frame_to_chunk(
                                        frame,
                                        &id,
                                        &model,
                                        &mut emitted_role,
                                    );
                                    return Some((
                                        Ok(chunk),
                                        (s, state, id, model, emitted_role),
                                    ));
                                }
                            }
                        }
                        return None;
                    }
                }
            }
        },
    )
}

/// Line-buffered NDJSON parser state. Holds whatever bytes have
/// arrived but not yet been delimited by `\n`.
#[derive(Default)]
struct NdjsonState {
    buf: Vec<u8>,
}

impl NdjsonState {
    fn extend(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }
    fn next_line(&mut self) -> Option<String> {
        let pos = self.buf.iter().position(|&b| b == b'\n')?;
        let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
        // Drop trailing \n + optional \r.
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        String::from_utf8(line).ok()
    }
    fn flush_tail(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let tail = std::mem::take(&mut self.buf);
        String::from_utf8(tail).ok()
    }
}

/// Convert one Ollama frame to an OpenAI-flavored chunk. `emitted_role`
/// flips to `true` on the first chunk so we only declare the
/// `assistant` role once (matches OpenAI's wire behaviour and keeps
/// the upstream aggregator's role-coercion off).
fn frame_to_chunk(
    frame: OllamaStreamFrame,
    id: &str,
    model: &str,
    emitted_role: &mut bool,
) -> ChatStreamChunk {
    let role = if !*emitted_role {
        *emitted_role = true;
        Some(if frame.message.role.is_empty() {
            "assistant".to_owned()
        } else {
            frame.message.role.clone()
        })
    } else {
        None
    };

    let content = if frame.message.content.is_empty() {
        None
    } else {
        Some(frame.message.content)
    };

    // Tool calls in Ollama's stream typically arrive on the FINAL
    // frame as a fully-formed array, not as the per-arg-character
    // deltas OpenAI does. Forward each as a single delta with the
    // arguments already serialized — the upstream aggregator
    // concatenates `arguments` deltas, so emitting the whole thing
    // in one chunk produces the right final value.
    let tool_calls: Vec<ToolCallDelta> = frame
        .message
        .tool_calls
        .into_iter()
        .enumerate()
        .map(|(idx, tc)| ToolCallDelta {
            index: idx as u32,
            id: Some(tc.id.unwrap_or_else(|| format!("call_{idx}"))),
            kind: Some("function".to_owned()),
            function: Some(ToolCallFunctionDelta {
                name: Some(tc.function.name),
                arguments: Some(tc.function.arguments.to_string()),
            }),
        })
        .collect();

    let finish_reason = if frame.done {
        frame.done_reason.or_else(|| Some("stop".to_owned()))
    } else {
        None
    };

    ChatStreamChunk {
        id: id.to_owned(),
        model: model.to_owned(),
        choices: vec![ChatStreamChoice {
            index: 0,
            delta: ChatStreamDelta {
                role,
                content,
                tool_calls,
            },
            finish_reason,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FunctionDecl;

    #[test]
    fn daemon_root_strips_v1_suffix() {
        assert_eq!(daemon_root("http://127.0.0.1:8101/v1"), "http://127.0.0.1:8101");
        assert_eq!(daemon_root("http://127.0.0.1:8101/v1/"), "http://127.0.0.1:8101");
        assert_eq!(daemon_root("http://127.0.0.1:8101"), "http://127.0.0.1:8101");
        assert_eq!(daemon_root("http://localhost:8101/v1"), "http://localhost:8101");
    }

    #[test]
    fn build_request_translates_tools_and_options() {
        let req = ChatRequest {
            model: ModelId("qwen2.5:7b".into()),
            messages: vec![ChatMessage::user("hi")],
            tools: Some(vec![ToolDeclaration {
                kind: "function".into(),
                function: FunctionDecl {
                    name: "web_search".into(),
                    description: "Search the web".into(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": { "query": { "type": "string" } },
                        "required": ["query"]
                    }),
                },
            }]),
            stream: false,
            temperature: Some(0.3),
            max_tokens: Some(256),
            chat_template_kwargs: None,
            tool_choice: None,
            guided_decoding_backend: None,
        };
        let ollama_req = build_request(&req, false);
        let serialized = serde_json::to_value(&ollama_req).unwrap();
        assert_eq!(serialized["model"], "qwen2.5:7b");
        assert_eq!(serialized["stream"], false);
        // f32 → JSON round-trips with float drift (0.3_f32 →
        // 0.30000001…). Compare with a small epsilon.
        let temp = serialized["options"]["temperature"].as_f64().unwrap();
        assert!((temp - 0.3).abs() < 1e-5, "temperature drift: {temp}");
        assert_eq!(serialized["options"]["num_predict"], 256);
        // num_ctx is pinned on every request — see DEFAULT_NUM_CTX
        // doc for the rationale.
        assert_eq!(serialized["options"]["num_ctx"], DEFAULT_NUM_CTX);
        assert_eq!(serialized["tools"][0]["function"]["name"], "web_search");
        assert_eq!(serialized["messages"][0]["role"], "user");
        assert_eq!(serialized["messages"][0]["content"], "hi");
    }

    #[test]
    fn build_request_always_sets_num_ctx_to_keep_tools_in_window() {
        // Even when the caller doesn't specify temperature or
        // max_tokens, we must pin num_ctx so Ollama doesn't fall
        // back to its 4096-token default and truncate the agent's
        // tool catalog out of the middle of the prompt.
        let req = ChatRequest {
            model: ModelId("qwen2.5:7b".into()),
            messages: vec![ChatMessage::user("hi")],
            tools: None,
            stream: false,
            temperature: None,
            max_tokens: None,
            chat_template_kwargs: None,
            tool_choice: None,
            guided_decoding_backend: None,
        };
        let serialized = serde_json::to_value(build_request(&req, false)).unwrap();
        assert_eq!(
            serialized["options"]["num_ctx"], DEFAULT_NUM_CTX,
            "num_ctx must always be sent (default {DEFAULT_NUM_CTX}) — Ollama's 4096 default truncates tool schemas"
        );
        // The other fields stay optional and absent when not set.
        assert!(serialized["options"].get("temperature").is_none());
        assert!(serialized["options"].get("num_predict").is_none());
    }

    #[test]
    fn response_to_openai_surfaces_tool_calls_as_stringified_arguments() {
        // The exact wire shape Ollama returned in the live probe
        // that motivated this module — small qwen2.5 emits a clean
        // structured tool call on /api/chat but text on /v1/chat.
        let body = r#"{
            "model":"qwen2.5:3b-instruct-q4_K_M",
            "created_at":"2026-05-18T19:15:23Z",
            "message":{
                "role":"assistant",
                "content":"",
                "tool_calls":[{
                    "id":"call_3wfr1wbt",
                    "function":{
                        "index":0,
                        "name":"web_search",
                        "arguments":{"query":"weather in Vancouver"}
                    }
                }]
            },
            "done":true,
            "done_reason":"stop",
            "prompt_eval_count":151,
            "eval_count":22
        }"#;
        let raw: OllamaChatResponse = serde_json::from_str(body).unwrap();
        let chat = response_to_openai(raw, &ModelId("qwen2.5:3b-instruct-q4_K_M".into()));
        assert_eq!(chat.choices.len(), 1);
        let msg = &chat.choices[0].message;
        assert!(matches!(msg.role, Role::Assistant));
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_calls[0].id, "call_3wfr1wbt");
        assert_eq!(msg.tool_calls[0].function.name, "web_search");
        // Arguments MUST be a JSON STRING per OpenAI's contract —
        // the downstream parser will `serde_json::from_str(args)`.
        let parsed: serde_json::Value =
            serde_json::from_str(&msg.tool_calls[0].function.arguments).unwrap();
        assert_eq!(parsed["query"], "weather in Vancouver");
        assert_eq!(chat.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = chat.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 151);
        assert_eq!(usage.completion_tokens, 22);
        assert_eq!(usage.total_tokens, 173);
    }

    #[test]
    fn response_to_openai_handles_plain_text_reply_without_tool_calls() {
        let body = r#"{
            "model":"qwen2.5:7b",
            "message":{"role":"assistant","content":"Hello there"},
            "done":true,
            "done_reason":"stop",
            "prompt_eval_count":10,
            "eval_count":3
        }"#;
        let raw: OllamaChatResponse = serde_json::from_str(body).unwrap();
        let chat = response_to_openai(raw, &ModelId("qwen2.5:7b".into()));
        let msg = &chat.choices[0].message;
        assert!(msg.tool_calls.is_empty());
        match &msg.content {
            Some(crate::MessageContent::Text(t)) => assert_eq!(t, "Hello there"),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ndjson_to_chat_chunks_emits_role_once_then_content_then_tool_call() {
        // Synthesize Ollama's typical 3-frame stream: role+empty,
        // content, final frame with tool_calls + done.
        // Owned Vec so the stream doesn't borrow from a local
        // array (the unfold-based `ndjson_to_chat_chunks` requires
        // 'static items).
        let lines: Vec<String> = vec![
            r#"{"message":{"role":"assistant","content":""},"done":false}"#.into(),
            r#"{"message":{"role":"assistant","content":"Calling"},"done":false}"#.into(),
            r#"{"message":{"role":"assistant","content":"","tool_calls":[{"id":"call_x","function":{"name":"web_search","arguments":{"q":"VAN"}}}]},"done":true,"done_reason":"stop"}"#.into(),
        ];
        let bytes_iter: Vec<reqwest::Result<bytes::Bytes>> = lines
            .into_iter()
            .map(|l| {
                let mut v = l.into_bytes();
                v.push(b'\n');
                Ok(bytes::Bytes::from(v))
            })
            .collect();
        let s = futures::stream::iter(bytes_iter);
        let chunks_stream = ndjson_to_chat_chunks(s, "id-1".into(), "qwen2.5:7b".into());
        use futures::StreamExt;
        let chunks: Vec<_> = chunks_stream.collect().await;
        assert_eq!(chunks.len(), 3);
        let c0 = chunks[0].as_ref().unwrap();
        // First chunk carries the role.
        assert_eq!(c0.choices[0].delta.role.as_deref(), Some("assistant"));
        assert!(c0.choices[0].delta.content.is_none());
        // Second chunk has content, no role.
        let c1 = chunks[1].as_ref().unwrap();
        assert!(c1.choices[0].delta.role.is_none());
        assert_eq!(c1.choices[0].delta.content.as_deref(), Some("Calling"));
        // Third chunk carries the tool_call + finish_reason.
        let c2 = chunks[2].as_ref().unwrap();
        assert_eq!(c2.choices[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(c2.choices[0].delta.tool_calls.len(), 1);
        let tc = &c2.choices[0].delta.tool_calls[0];
        assert_eq!(tc.id.as_deref(), Some("call_x"));
        assert_eq!(
            tc.function.as_ref().unwrap().name.as_deref(),
            Some("web_search")
        );
        // Arguments stringified as JSON.
        let args_str = tc
            .function
            .as_ref()
            .unwrap()
            .arguments
            .as_deref()
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(args_str).unwrap();
        assert_eq!(parsed["q"], "VAN");
    }

    #[tokio::test]
    async fn ndjson_to_chat_chunks_handles_split_lines_across_byte_chunks() {
        // TCP reads can split a line in half. Make sure the buffer
        // re-assembles correctly before parsing.
        let frame = r#"{"message":{"role":"assistant","content":"hi"},"done":true,"done_reason":"stop"}"#;
        let with_newline = format!("{frame}\n");
        let bytes = with_newline.as_bytes();
        // Split right in the middle of the JSON.
        let mid = bytes.len() / 2;
        let s = futures::stream::iter(vec![
            Ok::<_, reqwest::Error>(bytes::Bytes::copy_from_slice(&bytes[..mid])),
            Ok(bytes::Bytes::copy_from_slice(&bytes[mid..])),
        ]);
        let chunks_stream = ndjson_to_chat_chunks(s, "id-1".into(), "m".into());
        use futures::StreamExt;
        let chunks: Vec<_> = chunks_stream.collect().await;
        assert_eq!(chunks.len(), 1);
        let c = chunks[0].as_ref().unwrap();
        assert_eq!(c.choices[0].delta.content.as_deref(), Some("hi"));
        assert_eq!(c.choices[0].finish_reason.as_deref(), Some("stop"));
    }
}
