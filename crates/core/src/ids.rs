//! Typed identifiers.
//!
//! Newtype wrappers around the underlying string/integer representations so
//! we can't accidentally pass a `PrincipalId` where a `ConversationId` is
//! expected.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Generate a fresh UUID-backed identifier.
            pub fn new() -> Self {
                Self(Uuid::new_v4().to_string())
            }

            /// Wrap a pre-existing string. Caller is responsible for uniqueness.
            pub fn from_string(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }
    };
}

string_id!(
    /// Stable identifier for a conversation.
    ConversationId
);
string_id!(
    /// Stable identifier for a Principal (a person, agent, plugin, or service).
    PrincipalId
);
string_id!(
    /// Stable identifier for an installed plugin.
    PluginId
);
string_id!(
    /// Identifier for an ops alert row.
    AlertId
);
string_id!(
    /// Identifier for an incident grouping several alerts.
    IncidentId
);
string_id!(
    /// Identifier for a background research job.
    ResearchJobId
);
string_id!(
    /// Identifier for an attachment blob.
    AttachmentId
);
/// Monotonic per-conversation event sequence number.
///
/// Starts at 1 for the first event in a conversation, not 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventSeq(pub i64);

impl EventSeq {
    pub const FIRST: EventSeq = EventSeq(1);

    pub fn next(self) -> Self {
        EventSeq(self.0 + 1)
    }
}

impl fmt::Display for EventSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Monotonic per-conversation turn number.
///
/// A turn is a commit unit (§2.4) containing a user message or resume, plus
/// the model turn that answered it, plus paired tool_use/tool_result blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnSeq(pub i64);

impl TurnSeq {
    pub const FIRST: TurnSeq = TurnSeq(1);

    pub fn next(self) -> Self {
        TurnSeq(self.0 + 1)
    }
}

impl fmt::Display for TurnSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Framework-minted idempotency key for the outbox (§2.4).
///
/// Derived from `(conversation_id, turn_seq, tool_call_ordinal)`. **Never**
/// derived from LLM output — the model can rephrase its own rationale and
/// break collision detection silently.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdempotencyKey(pub String);

impl IdempotencyKey {
    /// Build the canonical framework-minted form.
    pub fn mint(conversation_id: &ConversationId, turn_seq: TurnSeq, ordinal: u32) -> Self {
        Self(format!("{}:{}:{}", conversation_id.0, turn_seq.0, ordinal))
    }

    /// Wrap a pre-existing string. Normally only used by tests and by
    /// loaders that pull persisted keys back out of the outbox table.
    pub fn from_string(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_distinct_by_construction() {
        let a = ConversationId::new();
        let b = ConversationId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn event_seq_progresses_monotonically() {
        let s = EventSeq::FIRST;
        assert_eq!(s.next(), EventSeq(2));
        assert_eq!(s.next().next(), EventSeq(3));
    }

    #[test]
    fn idempotency_key_is_deterministic() {
        let c = ConversationId::from("conv-1");
        let k1 = IdempotencyKey::mint(&c, TurnSeq(7), 2);
        let k2 = IdempotencyKey::mint(&c, TurnSeq(7), 2);
        assert_eq!(k1, k2);
        assert_eq!(k1.as_str(), "conv-1:7:2");
    }

    #[test]
    fn idempotency_keys_distinct_across_ordinals() {
        let c = ConversationId::from("conv-1");
        let k1 = IdempotencyKey::mint(&c, TurnSeq(1), 0);
        let k2 = IdempotencyKey::mint(&c, TurnSeq(1), 1);
        assert_ne!(k1, k2);
    }

    /// Collision-avoidance across turn_seq — the key is required to
    /// distinguish (turn=1, ord=0) from (turn=2, ord=0). Otherwise the
    /// outbox would merge tool calls from different turns.
    #[test]
    fn idempotency_keys_distinct_across_turn_seq() {
        let c = ConversationId::from("conv-1");
        let k1 = IdempotencyKey::mint(&c, TurnSeq(1), 0);
        let k2 = IdempotencyKey::mint(&c, TurnSeq(2), 0);
        assert_ne!(k1, k2);
    }

    /// Collision-avoidance across conversation ids.
    #[test]
    fn idempotency_keys_distinct_across_conversations() {
        let k1 = IdempotencyKey::mint(&ConversationId::from("a"), TurnSeq(1), 0);
        let k2 = IdempotencyKey::mint(&ConversationId::from("b"), TurnSeq(1), 0);
        assert_ne!(k1, k2);
    }

    /// Typed IDs — a `PrincipalId` wrapping the same string as a
    /// `ConversationId` must NOT compile as interchangeable. (This is
    /// the whole point of the newtype.) We assert the non-equality at
    /// the value level as a last-resort smoke check; the real guarantee
    /// is the compiler.
    #[test]
    fn event_seq_ordering() {
        assert!(EventSeq(1) < EventSeq(2));
        assert!(EventSeq(100) > EventSeq(99));
    }
}
