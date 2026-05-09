//! Capability-token claim shape (§7.2).
//!
//! The actual EdDSA signing/verification lives in `server` or `vault`
//! depending on where the controller keypair lives. This module just owns
//! the claim shape so every consumer agrees on the fields.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CapabilityTokenClaims {
    pub conversation_id: String,
    pub turn_seq: i64,
    pub principal_id: String,
    pub capability_set: Vec<String>,
    /// Unix-seconds expiry.
    pub exp: i64,
    pub nonce: String,
}

#[derive(Debug, Error)]
pub enum CapabilityTokenError {
    #[error("token expired")]
    Expired,
    #[error("turn seq does not match current open turn (got {got}, expected {want})")]
    TurnMismatch { got: i64, want: i64 },
    #[error("missing capability: {0}")]
    MissingCapability(String),
    #[error("signature verification failed")]
    BadSignature,
}

impl CapabilityTokenClaims {
    pub fn has(&self, cap: &str) -> bool {
        self.capability_set.iter().any(|c| c == cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_checks_membership() {
        let c = CapabilityTokenClaims {
            conversation_id: "c1".into(),
            turn_seq: 1,
            principal_id: "p".into(),
            capability_set: vec!["transport.send".into(), "vault.read".into()],
            exp: 9_999_999_999,
            nonce: "abc".into(),
        };
        assert!(c.has("vault.read"));
        assert!(!c.has("vault.write"));
    }

    #[test]
    fn claims_roundtrip_json() {
        let c = CapabilityTokenClaims {
            conversation_id: "c".into(),
            turn_seq: 3,
            principal_id: "p".into(),
            capability_set: vec!["x".into()],
            exp: 0,
            nonce: "n".into(),
        };
        let s = serde_json::to_string(&c).unwrap();
        let back: CapabilityTokenClaims = serde_json::from_str(&s).unwrap();
        assert_eq!(back, c);
    }
}
