//! Event envelope (M6 event-driven architecture).
//!
//! Every event flowing through the [`automation_bus`] carries an
//! [`EventEnvelope`] that captures who sent it, where to reply, how
//! to correlate it with related events, and whether the producer
//! expects a reply at all. Plugins populate the envelope at publish
//! time; the executor reads it to route `SendReply` outputs and to
//! gate flows that declare `expects_reply`.
//!
//! The envelope is intentionally *plugin-opaque* for the reply
//! target: the runtime never inspects `channel_ref`. It hands the
//! blob back to the producing plugin's registered reply handler,
//! which is the only code that knows how to interpret it.
//!
//! Persistence: the envelope rides on `state_bus_events.envelope_json`
//! (added via migration 0014). Existing rows from before the
//! migration default to `EventEnvelope::system_internal()` so flows
//! that match historical events still run with sensible defaults.
//!
//! Design references:
//!   * Pipedream's `$.respond()` — capability injected by the trigger
//!   * Slack Bolt's context (`say`/`respond`/`client`) — context
//!     decided per event kind
//!   * Matrix appservice RoomBridgeStore — *persistent* mapping that
//!     survives restarts (we persist `OriginRef`, never trust an
//!     in-memory cache)
//!
//! See `docs/automations-event-driven.md` §3 for the full design.
//!
//! [`automation_bus`]: crate::automation_bus

use crate::ids::PrincipalId;
use crate::principal::TrustLevel;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Lightweight, copy-able flat-tag form of [`TrustLevel`] suitable
/// for the envelope (the rich [`TrustLevel`] enum carries
/// `Delegated { by, scope, expires_at }` etc. — too heavy + too
/// volatile to embed in every event row).
///
/// Round-trips to/from `TrustLevel::class_tag()`. The full trust
/// state lives in the principals table; the envelope carries only
/// what's needed to gate flows at publish time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum TrustClass {
    Controller,
    Delegated,
    KnownTrusted,
    KnownLimited,
    UnknownPending,
    ColdContact,
    Blocked,
}

impl TrustClass {
    pub fn from_level(level: &TrustLevel) -> Self {
        match level.class_tag() {
            "Controller" => Self::Controller,
            "Delegated" => Self::Delegated,
            "KnownTrusted" => Self::KnownTrusted,
            "KnownLimited" => Self::KnownLimited,
            "UnknownPending" => Self::ColdContact, // alias
            "Blocked" => Self::Blocked,
            _ => Self::UnknownPending,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Controller => "Controller",
            Self::Delegated => "Delegated",
            Self::KnownTrusted => "KnownTrusted",
            Self::KnownLimited => "KnownLimited",
            Self::UnknownPending => "UnknownPending",
            Self::ColdContact => "ColdContact",
            Self::Blocked => "Blocked",
        }
    }

    /// Reverse of [`as_str`]. Accepts both the canonical PascalCase
    /// form (what serde emits) and operator-friendly snake_case
    /// aliases that the SPA + flow-config UX exposes — so
    /// `"Controller"` and `"controller"` both round-trip cleanly.
    /// `KnownTrusted` also has the alias `known_high` to mirror the
    /// SPA's TypeScript TrustClass union which uses that label.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            // Canonical PascalCase
            "Controller" => Some(Self::Controller),
            "Delegated" => Some(Self::Delegated),
            "KnownTrusted" => Some(Self::KnownTrusted),
            "KnownLimited" => Some(Self::KnownLimited),
            "UnknownPending" => Some(Self::UnknownPending),
            "ColdContact" => Some(Self::ColdContact),
            "Blocked" => Some(Self::Blocked),
            // Operator-friendly snake_case (matches SPA TypeScript)
            "controller" => Some(Self::Controller),
            "delegated" => Some(Self::Delegated),
            "known_trusted" | "known_high" => Some(Self::KnownTrusted),
            "known_limited" => Some(Self::KnownLimited),
            "unknown_pending" => Some(Self::UnknownPending),
            "cold_contact" => Some(Self::ColdContact),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

/// How to reply to this event. `None` means the event is
/// fire-and-forget — the validator rejects flows that attach a
/// `SendReply` node to a trigger whose registered kind declares
/// `expects_reply = false`.
///
/// The runtime treats `channel_ref` as opaque. Only the producing
/// plugin (`plugin_id`) knows the channel-specific shape — it might
/// be `{ chat_id, thread_id }` for WhatsApp, `{ session_id, ws }`
/// for the web UI, `{ message_id, response_url }` for Slack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OriginRef {
    /// Web SPA WebSocket session. Replies stream via UiEvent::*.
    /// Built-in handler — not a plugin.
    WebSocketSession { session_id: String },

    /// Generic channel plugin (whatsapp, signal, sms, email, ...).
    /// Reply handler = `<plugin_id>.send_reply` per plugin manifest.
    PluginChannel {
        plugin_id: String,
        /// Plugin-opaque — never inspected by the runtime.
        #[schema(value_type = Object)]
        channel_ref: serde_json::Value,
        /// Ms since epoch. `None` = no expiry. When set and we miss
        /// the window, the router fires `OriginExpired` instead of
        /// trying the transport (a Slack `response_url` is the
        /// canonical motivator for this — 30-min TTL).
        expires_at: Option<i64>,
    },

    /// Append the agent's text to an existing chat thread. Used by
    /// `Notify`-style flows that surface output as a conversation
    /// entry, by the fallback ladder when transport delivery fails,
    /// and by briefing flows that drop output into the operator's
    /// Inbox.
    ChatAppend { conversation_id: String },

    /// Surface as an alert in the operator's alert dropdown.
    Alert,

    /// Drop. Used by `Notify`-only flows and by explicit
    /// `SendReply { target_override: None }` (story 6 — the bank
    /// email that we don't want to reply to).
    None,
}

impl OriginRef {
    /// Short label for logs/traces. Never include the channel_ref
    /// blob here — it may carry user-identifying info (chat ids).
    pub fn label(&self) -> &'static str {
        match self {
            OriginRef::WebSocketSession { .. } => "ws",
            OriginRef::PluginChannel { .. } => "plugin_channel",
            OriginRef::ChatAppend { .. } => "chat_append",
            OriginRef::Alert => "alert",
            OriginRef::None => "none",
        }
    }

    /// Stable fingerprint suitable for alert dedup. Excludes
    /// `channel_ref` (it may differ between two events that are
    /// "the same target" — e.g., two messages in the same chat).
    pub fn fingerprint(&self) -> String {
        match self {
            OriginRef::WebSocketSession { session_id } => format!("ws:{session_id}"),
            OriginRef::PluginChannel { plugin_id, .. } => format!("plugin:{plugin_id}"),
            OriginRef::ChatAppend { conversation_id } => format!("chat:{conversation_id}"),
            OriginRef::Alert => "alert".into(),
            OriginRef::None => "none".into(),
        }
    }

    /// `true` if a `SendReply` node could deliver to this origin.
    /// Used by the validator gate; `None`/`Alert` return false
    /// because they're *not* reply targets (Alert is reachable
    /// only via the `Notify` node).
    pub fn is_reply_target(&self) -> bool {
        matches!(
            self,
            OriginRef::WebSocketSession { .. }
                | OriginRef::PluginChannel { .. }
                | OriginRef::ChatAppend { .. }
        )
    }
}

/// Who sent the event. Trust classification on this field gates the
/// turn-policy engine; `External` with `ColdContact` trust triggers
/// the cold-contact approval flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SenderIdentity {
    /// Sender resolved to an existing principal (operator, known
    /// contact, etc.). The principal's stored trust level is the
    /// source of truth — the envelope's `trust` field is a snapshot
    /// at publish time used for fast filtering before the policy
    /// engine runs.
    Principal {
        #[schema(value_type = String)]
        id: PrincipalId,
        trust: TrustClass,
    },

    /// Plugin couldn't resolve the inbound sender to a stored
    /// principal — typically a cold contact arriving via a channel
    /// for the first time. The `handle` is the raw external
    /// identifier (phone number, email, slack user id) so the
    /// approval flow can show the operator something legible.
    External {
        plugin_id: String,
        handle: String,
        trust: TrustClass,
    },

    /// System-originated event (scheduled routine, internal wakeup,
    /// flow chaining another flow). No external sender; trust is
    /// implicitly Controller.
    System,
}

impl SenderIdentity {
    pub fn trust(&self) -> TrustClass {
        match self {
            SenderIdentity::Principal { trust, .. } => *trust,
            SenderIdentity::External { trust, .. } => *trust,
            SenderIdentity::System => TrustClass::Controller,
        }
    }
}

/// Cross-cutting metadata travelling with every event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct EventEnvelope {
    pub origin: OriginRef,
    pub identity: SenderIdentity,
    /// Stable id linking multiple events that belong to the same
    /// logical interaction (one inbound message + N tool calls + a
    /// reply all share a correlation_id). Plugin trigger
    /// implementations mint this; flows preserve it on emitted
    /// events.
    pub correlation_id: String,
    /// Set when the event was emitted by another flow (chained
    /// automation, internal `publish` from a Transform). The
    /// executor uses this to enforce a depth cap and to render the
    /// flow-run DAG in the Automations UI.
    pub parent_event_id: Option<String>,
}

impl EventEnvelope {
    /// Convenience: a no-reply system-internal envelope. Used by
    /// the historical migration shim, internal wakeups, and tests.
    pub fn system_internal() -> Self {
        Self {
            origin: OriginRef::None,
            identity: SenderIdentity::System,
            correlation_id: format!("sys-{}", uuid::Uuid::new_v4()),
            parent_event_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn origin_ref_serde_round_trips() {
        let cases = vec![
            OriginRef::WebSocketSession {
                session_id: "ses-1".into(),
            },
            OriginRef::PluginChannel {
                plugin_id: "whatsapp".into(),
                channel_ref: json!({"chat_id": "+15551234", "jid": "abc@s.whatsapp.net"}),
                expires_at: Some(1_700_000_000_000),
            },
            OriginRef::ChatAppend {
                conversation_id: "conv-7".into(),
            },
            OriginRef::Alert,
            OriginRef::None,
        ];
        for o in cases {
            let s = serde_json::to_string(&o).unwrap();
            let back: OriginRef = serde_json::from_str(&s).unwrap();
            assert_eq!(o, back, "round-trip: {s}");
        }
    }

    #[test]
    fn origin_ref_is_reply_target_classification() {
        assert!(OriginRef::WebSocketSession {
            session_id: "x".into()
        }
        .is_reply_target());
        assert!(OriginRef::PluginChannel {
            plugin_id: "w".into(),
            channel_ref: json!({}),
            expires_at: None
        }
        .is_reply_target());
        assert!(OriginRef::ChatAppend {
            conversation_id: "c".into()
        }
        .is_reply_target());
        assert!(!OriginRef::Alert.is_reply_target());
        assert!(!OriginRef::None.is_reply_target());
    }

    #[test]
    fn origin_ref_fingerprint_excludes_channel_ref() {
        let a = OriginRef::PluginChannel {
            plugin_id: "whatsapp".into(),
            channel_ref: json!({"chat_id": "1"}),
            expires_at: None,
        };
        let b = OriginRef::PluginChannel {
            plugin_id: "whatsapp".into(),
            channel_ref: json!({"chat_id": "2"}),
            expires_at: None,
        };
        // Same plugin, different chats → same fingerprint (alert dedup
        // groups by plugin, not chat — operator gets ONE "whatsapp
        // delivery failing" alert, not one per chat).
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn sender_identity_trust_resolves() {
        assert_eq!(SenderIdentity::System.trust(), TrustClass::Controller);
        assert_eq!(
            SenderIdentity::Principal {
                id: PrincipalId::new(),
                trust: TrustClass::KnownTrusted
            }
            .trust(),
            TrustClass::KnownTrusted
        );
        assert_eq!(
            SenderIdentity::External {
                plugin_id: "signal".into(),
                handle: "+15551234".into(),
                trust: TrustClass::ColdContact
            }
            .trust(),
            TrustClass::ColdContact
        );
    }

    #[test]
    fn envelope_system_internal_has_no_reply_target() {
        let e = EventEnvelope::system_internal();
        assert!(!e.origin.is_reply_target());
        assert!(matches!(e.identity, SenderIdentity::System));
        assert!(e.parent_event_id.is_none());
        assert!(e.correlation_id.starts_with("sys-"));
    }
}
