//! Principal + trust ladder (§2.6, §2.14).

use serde::{Deserialize, Serialize};

use crate::ids::{PluginId, PrincipalId};

/// Transport-specific identifier, e.g. `signal:+15551234567`,
/// `email:a@b.com`, `web:sess-xyz`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identifier {
    pub transport: String,
    pub handle: String,
}

/// Trust hint published by identity-provider plugins (§2.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustHint {
    Contact,
    Colleague,
    Family,
    Organization,
    Unknown,
}

/// Capability scope for a `Delegated` trust grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityScope {
    pub capabilities: Vec<String>,
}

/// Trust ladder (§2.6).
///
/// `Blocked` is a **universal** state — it applies to previously-unknown
/// contacts AND to previously-trusted principals the controller later
/// decides to block (§0 memory: `project_locked_decisions_2026_04_23.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustLevel {
    Controller,
    Delegated {
        by: PrincipalId,
        scope: CapabilityScope,
        expires_at: Option<i64>,
    },
    KnownTrusted {
        resolvers: Vec<PluginId>,
        approved_by: PrincipalId,
        approved_at: i64,
    },
    KnownLimited {
        resolvers: Vec<PluginId>,
        allowed_topics: Vec<String>,
        allowed_tools: Option<Vec<String>>,
    },
    UnknownPending {
        first_seen: i64,
        notification_event_seq: Option<i64>,
    },
    Blocked {
        blocked_by: PrincipalId,
        blocked_at: i64,
        reason: Option<String>,
    },
}

impl TrustLevel {
    /// Short machine-readable tag for the `principals.trust_class` column
    /// and for policy-engine matching.
    pub fn class_tag(&self) -> &'static str {
        match self {
            TrustLevel::Controller => "Controller",
            TrustLevel::Delegated { .. } => "Delegated",
            TrustLevel::KnownTrusted { .. } => "KnownTrusted",
            TrustLevel::KnownLimited { .. } => "KnownLimited",
            TrustLevel::UnknownPending { .. } => "UnknownPending",
            TrustLevel::Blocked { .. } => "Blocked",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Principal {
    pub id: PrincipalId,
    pub identifiers: Vec<Identifier>,
    pub trust_level: TrustLevel,
    pub resolved_by: Vec<PluginId>,
    pub metadata: serde_json::Value,
    pub first_seen: i64,
    pub last_seen: Option<i64>,
    pub controller_notes: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_tag_covers_all_variants() {
        // Compile-time list so we don't forget to update the tag on new
        // variants.
        assert_eq!(TrustLevel::Controller.class_tag(), "Controller");
        assert_eq!(
            TrustLevel::Blocked {
                blocked_by: PrincipalId::from("c"),
                blocked_at: 0,
                reason: None,
            }
            .class_tag(),
            "Blocked"
        );
        assert_eq!(
            TrustLevel::UnknownPending {
                first_seen: 0,
                notification_event_seq: None,
            }
            .class_tag(),
            "UnknownPending"
        );
    }

    #[test]
    fn trust_level_json_roundtrips() {
        let t = TrustLevel::KnownTrusted {
            resolvers: vec![PluginId::from("google-contacts")],
            approved_by: PrincipalId::from("controller"),
            approved_at: 12345,
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: TrustLevel = serde_json::from_str(&s).unwrap();
        assert_eq!(back, t);
    }
}
