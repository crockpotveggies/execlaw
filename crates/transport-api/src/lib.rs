//! execlaw-transport-api
//!
//! Trait definitions a transport plugin implements (receive, send, identity
//! mapping). Phase 0 ships the `Transport` trait outline so downstream
//! crates can typecheck. Full contract (including streaming inbound, retry
//! semantics, idempotency interface) lands with `plugin-host` in Phase 2.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use execlaw_core::ids::ConversationId;
use serde::{Deserialize, Serialize};

/// An inbound or outbound conversation event on a transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationEvent {
    pub conversation_id: ConversationId,
    pub sender: String,
    pub text: Option<String>,
}

/// Trait every transport plugin implements.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, event: ConversationEvent) -> anyhow::Result<()>;
}
