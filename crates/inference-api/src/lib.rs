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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
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
            content: Some(content.into()),
            tool_call_id: None,
            name: None,
            tool_calls: Vec::new(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            tool_call_id: None,
            name: None,
            tool_calls: Vec::new(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            tool_call_id: None,
            name: None,
            tool_calls: Vec::new(),
        }
    }
    pub fn tool_result(tool_call_id: impl Into<String>, result_json: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(result_json.into()),
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
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("reqwest client build"),
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
    /// flag — streaming uses `chat_completions_stream` (future Phase 1
    /// follow-up).
    pub async fn chat_completions(
        &self,
        req: &ChatRequest,
    ) -> Result<ChatResponse, InferenceError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut r = self.http.post(&url).json(&ChatRequestNonStreaming(req));
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
        serde_json::from_str::<ChatResponse>(&text).map_err(|e| {
            InferenceError::Decode(format!(
                "bad /v1/chat/completions response: {e} — body: {text}"
            ))
        })
    }
}

/// Serializer adapter that forces `stream = false` regardless of input.
/// Ensures non-streaming calls don't get SSE back by accident.
struct ChatRequestNonStreaming<'a>(&'a ChatRequest);

impl<'a> Serialize for ChatRequestNonStreaming<'a> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("ChatRequest", 6)?;
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
        };
        let resp = client.chat_completions(&req).await.unwrap();
        assert_eq!(resp.id, "test-1");
        assert_eq!(
            resp.choices[0].message.content.as_deref(),
            Some("hello back")
        );
        let _ = handle.await;
    }
}
