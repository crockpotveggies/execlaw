//! execlaw-inference-api
//!
//! The single internal contract for LLM access. Anything in the workspace
//! that wants to talk to a model goes through this crate, which speaks an
//! **OpenAI-compatible API** (§2.8, §3.2).
//!
//! **No cloud-vendor SDKs.** Not `anthropic-sdk`, not `openai`, not
//! `google-genai`. The endpoint this client talks to is always a local
//! inference server — vLLM, OpenArc, llama.cpp server, Ollama — or an
//! operator-opted-in inference-bridge plugin running locally. That rule is
//! invariant (§0 axiom #1, 2026-04-23 locked decision).

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Model identifier as understood by the configured backend.
///
/// The execlaw default is `"QuantTrio/Qwen3.5-27B-AWQ"` per the 2026-04-23
/// locked decisions — but callers should not hardcode that; it's read from
/// the runner-deployment registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelId(pub String);

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
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
}

/// A single `/v1/chat/completions` request.
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

#[derive(Debug, Error)]
pub enum InferenceError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("backend returned status {status}: {body}")]
    BadStatus { status: u16, body: String },
}

/// Stub client.
///
/// Phase 0 only carries the types + an explicit "not wired yet" marker so
/// other crates can typecheck against `ChatRequest` / `ChatMessage`. The
/// real async streaming SSE client lands in Phase 1.
#[derive(Debug, Clone)]
pub struct InferenceClient {
    pub base_url: String,
    pub api_key: Option<String>,
}

impl InferenceClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: None,
        }
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_request_serializes_openai_shape() {
        let req = ChatRequest {
            model: ModelId("QuantTrio/Qwen3.5-27B-AWQ".to_owned()),
            messages: vec![
                ChatMessage {
                    role: Role::System,
                    content: "you are execlaw".into(),
                    tool_call_id: None,
                    name: None,
                },
                ChatMessage {
                    role: Role::User,
                    content: "hi".into(),
                    tool_call_id: None,
                    name: None,
                },
            ],
            tools: Some(vec![ToolDeclaration {
                name: "read_memory".into(),
                description: "read a long-term memory entry".into(),
                parameters: json!({"type": "object"}),
            }]),
            stream: true,
            temperature: None,
            max_tokens: Some(512),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"model\""));
        assert!(s.contains("\"messages\""));
        assert!(s.contains("\"tools\""));
        assert!(s.contains("\"stream\":true"));
        // No cloud-vendor-specific fields.
        assert!(!s.contains("anthropic"));
        assert!(!s.contains("openai"));
    }

    #[test]
    fn client_defaults_to_no_api_key() {
        let c = InferenceClient::new("http://127.0.0.1:8000/v1");
        assert!(c.api_key.is_none());
    }
}
