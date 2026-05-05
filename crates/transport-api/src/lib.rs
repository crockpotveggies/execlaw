//! execlaw-transport-api
//!
//! Trait definitions a transport plugin implements (receive, send, identity
//! mapping). Phase 0 ships the `Transport` trait outline so downstream
//! crates can typecheck. Full contract (including streaming inbound, retry
//! semantics, idempotency interface) lands with `plugin-host` in Phase 2.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use execlaw_core::conversation::Phase;
use execlaw_core::ids::ConversationId;
use serde::{Deserialize, Serialize};

/// An inbound or outbound conversation event on a transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationEvent {
    pub conversation_id: ConversationId,
    pub sender: String,
    pub text: Option<String>,
}

/// Snapshot of a conversation's processing state, surfaced through
/// [`Transport::on_phase_changed`] so transports can drive their
/// presence indicators (Signal "is typing", voice "thinking" UI,
/// email — typically ignored). The `is_processing` field is the
/// derived boolean every consumer cares about; raw `phase` is
/// included for transports that want finer granularity (e.g. voice
/// could surface "running tool" differently from "thinking").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationPhaseUpdate {
    pub conversation_id: ConversationId,
    pub phase: Phase,
    /// Mirrors `Phase::is_processing()` so the transport doesn't
    /// need to keep its own classification table.
    pub is_processing: bool,
}

/// Trait every transport plugin implements.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, event: ConversationEvent) -> anyhow::Result<()>;

    /// Notification that a conversation's FSM phase changed.
    ///
    /// Transports use this to drive their presence indicator. The
    /// canonical mapping is:
    ///   * `update.is_processing == true`  → start typing /
    ///     thinking / processing UI.
    ///   * `update.is_processing == false` → stop the indicator.
    ///
    /// Default impl is a no-op so transports that don't surface
    /// presence (email, RSS-style polling) don't need to implement
    /// it. The plugin host fans phase updates out to every
    /// registered transport whose conversation-handle map matches
    /// `update.conversation_id`. Errors from this method are logged
    /// and swallowed — a transport that can't show typing dots is
    /// not a reason to abort the turn.
    async fn on_phase_changed(&self, _update: ConversationPhaseUpdate) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Default no-op `on_phase_changed` doesn't blow up, doesn't
    /// require implementing crates to override it, and lets a
    /// transport that genuinely overrides receive every update.
    struct NoopTransport;
    #[async_trait]
    impl Transport for NoopTransport {
        async fn send(&self, _: ConversationEvent) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct TypingTransport {
        typing_started: AtomicUsize,
        typing_stopped: AtomicUsize,
        last_was_processing: AtomicBool,
    }
    #[async_trait]
    impl Transport for TypingTransport {
        async fn send(&self, _: ConversationEvent) -> anyhow::Result<()> {
            Ok(())
        }
        async fn on_phase_changed(&self, update: ConversationPhaseUpdate) -> anyhow::Result<()> {
            self.last_was_processing
                .store(update.is_processing, Ordering::SeqCst);
            if update.is_processing {
                self.typing_started.fetch_add(1, Ordering::SeqCst);
            } else {
                self.typing_stopped.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn default_on_phase_changed_is_a_no_op() {
        let t = NoopTransport;
        let update = ConversationPhaseUpdate {
            conversation_id: ConversationId::from("c1"),
            phase: Phase::Thinking,
            is_processing: true,
        };
        // Just must not panic / error.
        t.on_phase_changed(update).await.unwrap();
    }

    #[tokio::test]
    async fn override_receives_processing_lifecycle_in_order() {
        let t = TypingTransport {
            typing_started: AtomicUsize::new(0),
            typing_stopped: AtomicUsize::new(0),
            last_was_processing: AtomicBool::new(false),
        };

        // Start: phase=Thinking → is_processing=true.
        t.on_phase_changed(ConversationPhaseUpdate {
            conversation_id: ConversationId::from("c1"),
            phase: Phase::Thinking,
            is_processing: true,
        })
        .await
        .unwrap();
        assert_eq!(t.typing_started.load(Ordering::SeqCst), 1);
        assert_eq!(t.typing_stopped.load(Ordering::SeqCst), 0);

        // Tool boundary: phase=AwaitingTool → still processing.
        t.on_phase_changed(ConversationPhaseUpdate {
            conversation_id: ConversationId::from("c1"),
            phase: Phase::AwaitingTool,
            is_processing: true,
        })
        .await
        .unwrap();
        assert_eq!(
            t.typing_started.load(Ordering::SeqCst),
            2,
            "tool boundary keeps the indicator on (transport may dedupe internally)"
        );

        // End: phase=Idle → indicator off.
        t.on_phase_changed(ConversationPhaseUpdate {
            conversation_id: ConversationId::from("c1"),
            phase: Phase::Idle,
            is_processing: false,
        })
        .await
        .unwrap();
        assert_eq!(t.typing_stopped.load(Ordering::SeqCst), 1);
        assert!(!t.last_was_processing.load(Ordering::SeqCst));

        // AwaitingApproval: human-wait, NOT processing.
        t.on_phase_changed(ConversationPhaseUpdate {
            conversation_id: ConversationId::from("c1"),
            phase: Phase::AwaitingApproval,
            is_processing: false,
        })
        .await
        .unwrap();
        assert_eq!(t.typing_stopped.load(Ordering::SeqCst), 2);
    }
}
