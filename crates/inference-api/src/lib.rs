//! execlaw-inference-api
//!
//! The single internal contract for LLM access. Anything in the workspace
//! that wants to talk to a model goes through this crate, which speaks an
//! **OpenAI-compatible API** (§2.8, §3.2 of MIGRATION_PLAN.md).
//!
//! **No cloud-vendor SDKs. Ever.** Not `anthropic-sdk`, not `openai`, not
//! `google-genai`. The endpoint this client talks to is always a local
//! inference server — vLLM (default: `QuantTrio/Qwen3.5-27B-AWQ`), OpenArc,
//! llama.cpp server, Ollama. This rule is invariant (§0 axiom #1,
//! 2026-04-23 locked decisions).

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Model + chat types (OpenAI function-calling schema)
// ---------------------------------------------------------------------------

/// Model identifier as understood by the configured backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelId(pub String);

impl ModelId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Role of a chat message in OpenAI's function-calling schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Body of a chat message. OpenAI's chat schema accepts EITHER a plain
/// string OR an array of typed content parts (text + image_url for
/// vision-enabled models like Qwen3-VL / Qwen3.6 / LLaVA / Pixtral).
/// The untagged enum serialises to whichever shape matches the input
/// so existing text-only call sites are byte-identical on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl MessageContent {
    /// Extract the plain-text portion for logging / history projection.
    /// Concatenates the text parts of a parts-array with newlines; an
    /// image-only message returns the empty string.
    pub fn as_text(&self) -> String {
        match self {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    ContentPart::ImageUrl { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// One typed content part inside a parts-array `MessageContent`. The
/// `image_url` variant follows OpenAI's vision schema verbatim — Qwen
/// VL, LLaVA, Llama-3.2-Vision, Pixtral, and Phi-3.5-Vision all accept
/// it via the OpenAI-compatible bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    /// Either an https:// URL or a `data:image/<mime>;base64,<bytes>`
    /// data URL. execlaw uses the data-URL form so the inference
    /// backend doesn't need network access to fetch attachments.
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Set by the assistant when the model chose to call tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(MessageContent::Text(content.into())),
            tool_call_id: None,
            name: None,
            tool_calls: Vec::new(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(MessageContent::Text(content.into())),
            tool_call_id: None,
            name: None,
            tool_calls: Vec::new(),
        }
    }
    /// Build a user message with attached images. Each `images` entry
    /// is a data URL (e.g. `data:image/png;base64,...`). The text is
    /// emitted as the first content part; images follow in the order
    /// provided. An empty `text` is allowed (image-only message).
    pub fn user_with_images(
        text: impl Into<String>,
        images: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut parts: Vec<ContentPart> = Vec::new();
        let text = text.into();
        if !text.is_empty() {
            parts.push(ContentPart::Text { text });
        }
        for url in images {
            parts.push(ContentPart::ImageUrl {
                image_url: ImageUrl { url },
            });
        }
        Self {
            role: Role::User,
            content: Some(MessageContent::Parts(parts)),
            tool_call_id: None,
            name: None,
            tool_calls: Vec::new(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(MessageContent::Text(content.into())),
            tool_call_id: None,
            name: None,
            tool_calls: Vec::new(),
        }
    }
    pub fn tool_result(tool_call_id: impl Into<String>, result_json: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(MessageContent::Text(result_json.into())),
            tool_call_id: Some(tool_call_id.into()),
            name: None,
            tool_calls: Vec::new(),
        }
    }
}

/// A tool call emitted by the model (assistant role).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String, // always "function"
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// JSON-encoded arguments as a string per the OpenAI spec.
    pub arguments: String,
}

/// A tool the agent exposes to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDeclaration {
    #[serde(rename = "type")]
    pub kind: String, // always "function"
    pub function: FunctionDecl,
}

impl ToolDeclaration {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        params: serde_json::Value,
    ) -> Self {
        Self {
            kind: "function".into(),
            function: FunctionDecl {
                name: name.into(),
                description: description.into(),
                parameters: params,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDecl {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
}

/// A `/v1/chat/completions` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: ModelId,
    pub messages: Vec<ChatMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDeclaration>>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// 2026-04-28 — vLLM-extension knob, forwarded verbatim into the
    /// chat-template render via the `chat_template_kwargs` field on
    /// the OpenAI-compatible POST body. Qwen3.5 honours
    /// `{"enable_thinking": false}` to suppress its native `<think>`
    /// blocks; without it the local model emits a "Thinking Process:"
    /// monologue ahead of every reply. Other models silently ignore
    /// the field, so passing it unconditionally is safe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<serde_json::Value>,
}

/// `GET /v1/models` response shape (OpenAI list endpoint). vLLM,
/// llama.cpp server, Ollama, OpenArc all return this envelope; the
/// only field the SPA reads is `data[].id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelListResponse {
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub data: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub owned_by: Option<String>,
}

/// Heuristic check — does the given model id name a known
/// multimodal (vision-capable) family? Returns true on a match
/// against the curated pattern set; false otherwise.
///
/// Curated list (case-insensitive substring match):
///   * Qwen vision: `qwen2-vl`, `qwen2.5-vl`, `qwen3-vl`, `qwen3.6`
///   * LLaVA: `llava`, `llava-onevision`
///   * Llama 3.2 Vision: `llama-3.2-11b-vision`, `llama-3.2-90b-vision`
///   * Pixtral: `pixtral`
///   * Phi-3.5/Phi-4 vision: `phi-3.5-vision`, `phi-4-multimodal`
///   * MiniCPM-V: `minicpm-v`
///   * InternVL: `internvl`
///   * Generic suffixes operators commonly use: `-vision`, `-vl`,
///     `-multimodal`, `-mm`
///
/// New families land here as they ship. The probe is heuristic by
/// design — vLLM / llama.cpp's /v1/models response doesn't carry an
/// explicit multimodal flag, so id-pattern matching is the most
/// reliable signal available short of an actual image probe (which
/// would cost a real inference round-trip).
pub fn is_known_multimodal_model(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    const PATTERNS: &[&str] = &[
        // Qwen
        "qwen2-vl",
        "qwen2.5-vl",
        "qwen2_5-vl",
        "qwen3-vl",
        "qwen3.5-vl",
        "qwen3_5-vl",
        "qwen3.6",
        "qwen3_6",
        // LLaVA
        "llava",
        // Llama 3.2 vision
        "llama-3.2-11b-vision",
        "llama-3.2-90b-vision",
        "llama-3-vision",
        // Pixtral
        "pixtral",
        // Phi vision
        "phi-3.5-vision",
        "phi-3-vision",
        "phi-4-multimodal",
        // MiniCPM-V
        "minicpm-v",
        // InternVL
        "internvl",
        // Generic suffixes
        "-vision",
        "-multimodal",
    ];
    for p in PATTERNS {
        if id.contains(p) {
            return true;
        }
    }
    // Cheaper standalone token matches that benefit from word-boundary
    // checks to avoid false positives like "qwen2-7b" → matching "vl"
    // anywhere. Use suffix/segment guards.
    for tail in ["-vl", "_vl", "-mm"] {
        if id.ends_with(tail)
            || id.contains(&format!("{tail}-"))
            || id.contains(&format!("{tail}_"))
        {
            return true;
        }
    }
    false
}

/// Non-streaming response shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub id: String,
    pub model: String,
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: ChatMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum InferenceError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("backend returned status {status}: {body}")]
    BadStatus { status: u16, body: String },
    #[error("request timed out")]
    Timeout,
}

/// Construct the reqwest client every `InferenceClient::new` uses.
/// Centralised so the timeout / pool / keepalive knobs that bit
/// operators in production stay in one obvious place.
fn base_inference_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .pool_idle_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(120))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .build()
        .expect("reqwest client build")
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// An OpenAI-compatible inference client. Points at a LOCAL endpoint —
/// never a cloud provider. Typical `base_url` values:
///
/// - `http://127.0.0.1:8000/v1` — vLLM (default for execlaw's nvidia path)
/// - `http://127.0.0.1:8793/v1` — OpenArc (Intel GPU, used for voice stack)
/// - `http://127.0.0.1:11434/v1` — Ollama
#[derive(Debug, Clone)]
pub struct InferenceClient {
    pub base_url: String,
    pub api_key: Option<String>,
    http: reqwest::Client,
}

impl InferenceClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_client(
            base_url,
            // 2026-05-02 — reqwest's plain `.timeout()` covers
            // request-build + send + complete-response-read. For
            // streaming chat that's far too coarse: a long
            // multi-round agent conversation can exceed 120s on the
            // wire. Worse, the OLD client config bit operators
            // staring at a ~49s stall before a 500: reqwest's
            // connection pool would hand back a half-open keep-alive
            // socket vLLM had already closed during a backend
            // restart, and the client waited for the OS-level TCP
            // retransmit window before bailing. Now:
            //
            //   * `connect_timeout(10s)` → a half-open / unreachable
            //     vLLM fails in ~10s, not ~50s.
            //   * `pool_idle_timeout(15s)` → stale sockets are
            //     dropped from the pool 15s after their last use,
            //     well under the typical interval between turns,
            //     so the next turn opens a fresh connection.
            //   * `read_timeout(120s)` → guards against vLLM going
            //     silent mid-stream without holding the socket
            //     forever.
            //   * `tcp_keepalive(30s)` → kernel-side keepalive on
            //     long-lived agent turns surfaces a dead remote as
            //     a transport error rather than a hang.
            base_inference_http_client(),
        )
    }

    pub fn with_client(base_url: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: None,
            http,
        }
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Non-streaming chat completion.
    ///
    /// Request is sent with `stream = false` regardless of the `req.stream`
    /// flag — streaming uses [`chat_completions_stream`](Self::chat_completions_stream).
    pub async fn chat_completions(
        &self,
        req: &ChatRequest,
    ) -> Result<ChatResponse, InferenceError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        // 2026-05-12 — HTTP-layer timing instrumented on the
        // `agent::turn_timing` target so the operator can split
        // "vLLM is slow" from "client is slow" without correlating
        // by hand. `send_ms` = time to get response headers (which
        // for non-streaming usually means vLLM accepted the request
        // — generation hasn't started writing the body yet);
        // `body_ms` = headers → final body byte (this is where
        // generation latency lives for non-streaming, since vLLM
        // buffers the entire response server-side).
        let started_at = std::time::Instant::now();
        let mut r = self.http.post(&url).json(&ChatRequestNonStreaming(req));
        if let Some(key) = &self.api_key {
            r = r.bearer_auth(key);
        }
        let resp = r.send().await?;
        let send_ms = started_at.elapsed().as_millis() as u64;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::debug!(
                target: "agent::turn_timing",
                url = %url,
                send_ms,
                http_status = status.as_u16(),
                body_chars = body.chars().count(),
                "chat_completions non-streaming returned non-success status"
            );
            return Err(InferenceError::BadStatus {
                status: status.as_u16(),
                body,
            });
        }
        let body_started_at = std::time::Instant::now();
        let text = resp.text().await?;
        let body_ms = body_started_at.elapsed().as_millis() as u64;
        tracing::debug!(
            target: "agent::turn_timing",
            url = %url,
            send_ms,
            body_ms,
            response_chars = text.chars().count(),
            "chat_completions non-streaming HTTP round-trip"
        );
        serde_json::from_str::<ChatResponse>(&text).map_err(|e| {
            InferenceError::Decode(format!(
                "bad /v1/chat/completions response: {e} — body: {text}"
            ))
        })
    }

    /// Fetch the model list from `GET /v1/models` on the configured
    /// backend. Used by the SPA's multimodal-capability probe — the
    /// model id loaded into vLLM / Ollama / llama.cpp drives whether
    /// the chat composer surfaces an image-attach affordance.
    ///
    /// Returns the first model entry's id when present, alongside the
    /// raw JSON so future probes (e.g. context length) can read it
    /// without a second round-trip.
    pub async fn list_models(&self) -> Result<ModelListResponse, InferenceError> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let mut r = self.http.get(&url);
        if let Some(key) = &self.api_key {
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
        serde_json::from_str::<ModelListResponse>(&text).map_err(|e| {
            InferenceError::Decode(format!("bad /v1/models response: {e} — body: {text}"))
        })
    }

    /// Streaming chat completion.
    ///
    /// Sends `stream = true` and parses the OpenAI SSE format
    /// (`data: {json}\n\n`, terminated by `data: [DONE]`). Yields one
    /// [`ChatStreamChunk`] per SSE event. Invalid JSON inside a `data:`
    /// line is skipped with a warning — a single malformed chunk must
    /// not kill the whole stream.
    ///
    /// The caller aggregates chunk deltas into a full assistant
    /// message; streaming termination is signaled by the stream
    /// ending (no trailing `[DONE]` is yielded as a chunk).
    pub async fn chat_completions_stream(
        &self,
        req: &ChatRequest,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures::Stream<Item = Result<ChatStreamChunk, InferenceError>> + Send>,
        >,
        InferenceError,
    > {
        use futures::StreamExt;

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut r = self.http.post(&url).json(&ChatRequestStreaming(req));
        if let Some(key) = &self.api_key {
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

        // Raw byte stream from reqwest, framed into SSE events.
        let bytes_stream = resp.bytes_stream();
        let events = Box::pin(sse_parser::parse(
            bytes_stream.map(|r| r.map_err(InferenceError::from)),
        ));

        // For each SSE event, try to decode a ChatStreamChunk. Skip [DONE].
        let chunk_stream = events.filter_map(|ev| async move {
            match ev {
                Ok(SseEvent { data }) => {
                    let trimmed = data.trim();
                    if trimmed == "[DONE]" {
                        return None;
                    }
                    match serde_json::from_str::<ChatStreamChunk>(trimmed) {
                        Ok(c) => Some(Ok(c)),
                        Err(e) => Some(Err(InferenceError::Decode(format!(
                            "bad SSE chunk: {e} — body: {trimmed}"
                        )))),
                    }
                }
                Err(e) => Some(Err(e)),
            }
        });

        Ok(Box::pin(chunk_stream))
    }
}

// ---------------------------------------------------------------------------
// Streaming chunk types (OpenAI SSE shape)
// ---------------------------------------------------------------------------

/// One SSE event from `/v1/chat/completions?stream=true`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatStreamChunk {
    pub id: String,
    pub model: String,
    pub choices: Vec<ChatStreamChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatStreamChoice {
    pub index: u32,
    pub delta: ChatStreamDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatStreamDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Tool-call deltas are serialized across chunks per the OpenAI
    /// spec: the first delta has the `id` + `type` + `function.name`,
    /// subsequent deltas have `function.arguments` appended.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<ToolCallFunctionDelta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunctionDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

/// Serializer adapter that forces `stream = true` for streaming calls.
struct ChatRequestStreaming<'a>(&'a ChatRequest);

impl<'a> Serialize for ChatRequestStreaming<'a> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("ChatRequest", 7)?;
        st.serialize_field("model", &self.0.model)?;
        st.serialize_field("messages", &self.0.messages)?;
        if let Some(tools) = &self.0.tools {
            st.serialize_field("tools", tools)?;
        }
        st.serialize_field("stream", &true)?;
        if let Some(t) = &self.0.temperature {
            st.serialize_field("temperature", t)?;
        }
        if let Some(m) = &self.0.max_tokens {
            st.serialize_field("max_tokens", m)?;
        }
        if let Some(kw) = &self.0.chat_template_kwargs {
            st.serialize_field("chat_template_kwargs", kw)?;
        }
        st.end()
    }
}

// ---------------------------------------------------------------------------
// Minimal SSE parser — OpenAI's wire uses `data: {json}\n\n`. We don't
// need event types or IDs, just the `data:` payloads.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SseEvent {
    data: String,
}

mod sse_parser {
    use super::{InferenceError, SseEvent};
    use futures::{Stream, StreamExt};

    /// Frame a byte stream into one [`SseEvent`] per `\n\n`-separated
    /// block. Only the `data:` lines are collected (joined with `\n`),
    /// matching the OpenAI wire protocol.
    pub fn parse<S>(
        bytes: S,
    ) -> impl Stream<Item = Result<SseEvent, InferenceError>> + Send + 'static
    where
        S: Stream<Item = Result<bytes::Bytes, InferenceError>> + Send + 'static,
    {
        async_stream::stream! {
            let mut buf = String::new();
            let mut bytes = Box::pin(bytes);
            while let Some(chunk) = bytes.next().await {
                match chunk {
                    Ok(b) => {
                        buf.push_str(&String::from_utf8_lossy(&b));
                        // Yield any complete events in the buffer.
                        while let Some(idx) = buf.find("\n\n") {
                            let event_block: String = buf.drain(..idx + 2).collect();
                            if let Some(ev) = extract_data(&event_block) {
                                yield Ok(SseEvent { data: ev });
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                }
            }
            // Flush any tail event that didn't end with \n\n.
            if !buf.trim().is_empty() {
                if let Some(ev) = extract_data(&buf) {
                    yield Ok(SseEvent { data: ev });
                }
            }
        }
    }

    fn extract_data(block: &str) -> Option<String> {
        let mut data = String::new();
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start());
            }
        }
        if data.is_empty() { None } else { Some(data) }
    }
}

/// Serializer adapter that forces `stream = false` regardless of input.
/// Ensures non-streaming calls don't get SSE back by accident.
struct ChatRequestNonStreaming<'a>(&'a ChatRequest);

impl<'a> Serialize for ChatRequestNonStreaming<'a> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("ChatRequest", 7)?;
        st.serialize_field("model", &self.0.model)?;
        st.serialize_field("messages", &self.0.messages)?;
        if let Some(tools) = &self.0.tools {
            st.serialize_field("tools", tools)?;
        }
        st.serialize_field("stream", &false)?;
        if let Some(t) = &self.0.temperature {
            st.serialize_field("temperature", t)?;
        }
        if let Some(m) = &self.0.max_tokens {
            st.serialize_field("max_tokens", m)?;
        }
        if let Some(kw) = &self.0.chat_template_kwargs {
            st.serialize_field("chat_template_kwargs", kw)?;
        }
        st.end()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_request_serializes_openai_shape() {
        let req = ChatRequest {
            model: ModelId("QuantTrio/Qwen3.5-27B-AWQ".to_owned()),
            messages: vec![
                ChatMessage::system("you are execlaw"),
                ChatMessage::user("hi"),
            ],
            tools: Some(vec![ToolDeclaration::function(
                "read_memory",
                "read a long-term memory entry",
                json!({"type": "object"}),
            )]),
            stream: true,
            temperature: None,
            max_tokens: Some(512),
            chat_template_kwargs: None,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"model\""));
        assert!(s.contains("\"messages\""));
        assert!(s.contains("\"tools\""));
        // No cloud-vendor-specific fields.
        assert!(!s.contains("anthropic"));
        assert!(!s.to_lowercase().contains("gemini"));
    }

    #[test]
    fn client_defaults_to_no_api_key() {
        let c = InferenceClient::new("http://127.0.0.1:8000/v1");
        assert!(c.api_key.is_none());
    }

    #[test]
    fn chat_response_decodes_tool_calls() {
        let json_str = r#"{
            "id": "abc",
            "model": "Qwen3.5-27B-AWQ",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "read_memory",
                            "arguments": "{\"scope\":\"global\",\"key\":\"x\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let resp: ChatResponse = serde_json::from_str(json_str).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].message.tool_calls.len(), 1);
        assert_eq!(
            resp.choices[0].message.tool_calls[0].function.name,
            "read_memory"
        );
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn tool_result_message_round_trips() {
        let m = ChatMessage::tool_result("call_1", r#"{"value":"bf_emma"}"#);
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"role\":\"tool\""));
        assert!(s.contains("\"tool_call_id\":\"call_1\""));
    }

    /// SSE parser must collect `data:` lines between `\n\n` boundaries
    /// and skip the terminal `[DONE]`.
    #[tokio::test]
    async fn streaming_parses_openai_sse_frames() {
        use futures::StreamExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            use tokio::io::AsyncWriteExt;
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ =
                tokio::time::timeout(std::time::Duration::from_millis(500), sock.read(&mut buf))
                    .await;
            let body_chunks = [
                "data: {\"id\":\"a\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"He\"}}]}\n\n",
                "data: {\"id\":\"a\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"}}]}\n\n",
                "data: {\"id\":\"a\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            ];
            let body: String = body_chunks.join("");
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 content-type: text/event-stream\r\n\
                 content-length: {}\r\n\
                 connection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.flush().await;
        });

        let client = InferenceClient::new(format!("http://{addr}/v1"));
        let req = ChatRequest {
            model: ModelId("m".into()),
            messages: vec![ChatMessage::user("hi")],
            tools: None,
            stream: true,
            temperature: None,
            max_tokens: None,
            chat_template_kwargs: None,
        };
        let mut stream = client.chat_completions_stream(&req).await.unwrap();
        let mut text = String::new();
        let mut finished = false;
        while let Some(chunk) = stream.next().await {
            let c = chunk.unwrap();
            for ch in &c.choices {
                if let Some(t) = &ch.delta.content {
                    text.push_str(t);
                }
                if ch.finish_reason.is_some() {
                    finished = true;
                }
            }
        }
        assert_eq!(text, "Hello");
        assert!(finished, "expected finish_reason on a chunk");
    }

    /// A malformed `data:` line must NOT poison the whole stream — the
    /// consumer gets an `Err` for that chunk but subsequent chunks
    /// still flow.
    #[tokio::test]
    async fn streaming_surfaces_per_chunk_decode_errors() {
        use futures::StreamExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            use tokio::io::AsyncWriteExt;
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ =
                tokio::time::timeout(std::time::Duration::from_millis(500), sock.read(&mut buf))
                    .await;
            let body = String::from(
                "data: not-json\n\n\
                 data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n\
                 data: [DONE]\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(response.as_bytes()).await;
        });

        let client = InferenceClient::new(format!("http://{addr}/v1"));
        let req = ChatRequest {
            model: ModelId("m".into()),
            messages: vec![ChatMessage::user("hi")],
            tools: None,
            stream: true,
            temperature: None,
            max_tokens: None,
            chat_template_kwargs: None,
        };
        let mut stream = client.chat_completions_stream(&req).await.unwrap();
        let mut saw_err = false;
        let mut ok_content = String::new();
        while let Some(c) = stream.next().await {
            match c {
                Err(_) => saw_err = true,
                Ok(chunk) => {
                    for ch in chunk.choices {
                        if let Some(t) = ch.delta.content {
                            ok_content.push_str(&t);
                        }
                    }
                }
            }
        }
        assert!(saw_err, "expected a decode error for malformed chunk");
        assert_eq!(ok_content, "ok", "subsequent chunks must still stream");
    }

    /// Integration-style test: spin up a tokio TCP listener that pretends to
    /// be an OpenAI-compatible endpoint and verify the client serializes
    /// correctly + parses the canned response.
    #[tokio::test]
    async fn end_to_end_chat_completion_against_mock_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let canned = r#"{
            "id": "test-1",
            "model": "Qwen3.5-27B-AWQ",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello back"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
        }"#;

        let handle = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            use tokio::io::AsyncWriteExt;
            let (mut sock, _) = listener.accept().await.unwrap();
            // Read the request headers+body (enough bytes to clear the buffer).
            let mut buf = [0u8; 4096];
            let _ =
                tokio::time::timeout(std::time::Duration::from_millis(500), sock.read(&mut buf))
                    .await;
            let body = canned.as_bytes();
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 content-type: application/json\r\n\
                 content-length: {}\r\n\
                 connection: close\r\n\r\n{}",
                body.len(),
                canned
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.flush().await;
        });

        let client = InferenceClient::new(format!("http://{addr}/v1"));
        let req = ChatRequest {
            model: ModelId("QuantTrio/Qwen3.5-27B-AWQ".to_owned()),
            messages: vec![ChatMessage::user("hello")],
            tools: None,
            stream: false,
            temperature: Some(0.0),
            max_tokens: Some(16),
            chat_template_kwargs: None,
        };
        let resp = client.chat_completions(&req).await.unwrap();
        assert_eq!(resp.id, "test-1");
        assert_eq!(
            resp.choices[0]
                .message
                .content
                .as_ref()
                .map(|c| c.as_text()),
            Some("hello back".to_owned())
        );
        let _ = handle.await;
    }

    /// `MessageContent::Text` serialises as a plain string; the wire
    /// is byte-identical to the pre-vision shape so every existing
    /// backend keeps working.
    #[test]
    fn text_content_serialises_as_a_plain_string() {
        let m = ChatMessage::user("hi");
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"content\":\"hi\""), "got {s}");
        assert!(!s.contains("\"type\":\"text\""));
    }

    /// `MessageContent::Parts` serialises as OpenAI's vision content
    /// array — `[{type:"text",text:"..."},{type:"image_url",image_url:{url:"..."}}]`.
    /// This is what Qwen3-VL / Qwen3.6 / LLaVA / Pixtral expect.
    #[test]
    fn parts_content_serialises_as_openai_vision_array() {
        let m = ChatMessage::user_with_images(
            "describe this",
            vec!["data:image/png;base64,iVBOR".to_owned()],
        );
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"type\":\"text\""));
        assert!(s.contains("\"type\":\"image_url\""));
        assert!(s.contains("data:image/png;base64,iVBOR"));
    }

    #[test]
    fn known_multimodal_models_match() {
        for id in [
            "Qwen/Qwen2.5-VL-7B-Instruct",
            "Qwen/Qwen2-VL-72B-Instruct-AWQ",
            "Qwen/Qwen3-VL-32B",
            "Qwen3.6-27B-AWQ",
            "liuhaotian/llava-v1.6-mistral-7b",
            "meta-llama/Llama-3.2-11B-Vision-Instruct",
            "mistralai/Pixtral-12B-2409",
            "microsoft/Phi-3.5-vision-instruct",
            "microsoft/Phi-4-Multimodal-Instruct",
            "openbmb/MiniCPM-V-2_6",
            "OpenGVLab/InternVL2-26B",
        ] {
            assert!(
                is_known_multimodal_model(id),
                "expected {id} to be classified multimodal"
            );
        }
    }

    #[test]
    fn known_text_only_models_do_not_match() {
        for id in [
            "Qwen/Qwen3.5-27B-AWQ",
            "QuantTrio/Qwen3.5-27B-AWQ",
            "meta-llama/Llama-3.1-70B-Instruct",
            "mistralai/Mistral-7B-Instruct-v0.3",
            "google/gemma-2-27b-it",
            "openai/gpt-oss-20b",
        ] {
            assert!(
                !is_known_multimodal_model(id),
                "did not expect {id} to be classified multimodal"
            );
        }
    }

    /// Round-trip: an inbound parts-array deserialises back into a
    /// `Parts` variant (covers replay paths that feed assistant
    /// messages back into the LLM).
    #[test]
    fn parts_content_round_trips_through_serde() {
        let wire = serde_json::json!({
            "role": "user",
            "content": [
                {"type":"text","text":"hi"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,xyz"}},
            ],
        });
        let m: ChatMessage = serde_json::from_value(wire).unwrap();
        match m.content {
            Some(MessageContent::Parts(parts)) => {
                assert_eq!(parts.len(), 2);
            }
            other => panic!("expected Parts, got {other:?}"),
        }
    }
}
