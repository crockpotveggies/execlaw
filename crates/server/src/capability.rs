//! Per-turn capability tokens issued to runners.
//!
//! §7.2: "Every runner container launches with a short-lived, signed
//! EdDSA JWT. Token payload: `{ conversation_id, turn_seq, principal,
//! capability_set, exp, nonce }`. Bound to a specific turn, not the whole
//! session — the token issued for turn 47 cannot authorize actions claimed
//! as part of turn 48."
//!
//! We reuse the same Ed25519 keypair the user-facing JWTs use (in
//! [`crate::auth::JwtSigner`]) so runners can verify against the server's
//! public key via the standard `jsonwebtoken` crate.

use crate::auth::JwtSigner;
use serde::{Deserialize, Serialize};

/// Claims carried by a per-turn capability token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityClaims {
    /// Issuer — matches the server's configured `jwt_issuer`.
    pub iss: String,
    /// Subject — principal id of the runner this token is for.
    pub sub: String,
    /// Issued-at (seconds since epoch).
    pub iat: i64,
    /// Expiration (seconds since epoch). Capability tokens are
    /// deliberately short-lived; default 5 minutes.
    pub exp: i64,

    /// Conversation this token authorizes actions in.
    pub conversation_id: String,
    /// Turn within the conversation. A token for `turn_seq = 47` is
    /// REJECTED by the policy engine if presented for turn 48.
    pub turn_seq: i64,
    /// Capability set for the turn — e.g. `["tools.safe", "memory.read",
    /// "memory.write"]`. Wildcards allowed per §7.3.
    pub capability_set: Vec<String>,
    /// Random nonce for replay defense.
    pub nonce: String,
}

/// Default TTL for capability tokens.
pub const DEFAULT_CAPABILITY_TTL_SECS: i64 = 5 * 60;

/// Issue a capability token for a specific turn. Returns the JWT string.
pub fn issue_capability_token(
    signer: &JwtSigner,
    principal_id: &str,
    conversation_id: &str,
    turn_seq: i64,
    capability_set: Vec<String>,
    ttl_secs: Option<i64>,
) -> String {
    use jsonwebtoken::{Algorithm, Header, encode};
    use rand::RngCore;
    use rand::rngs::OsRng;

    let now = chrono::Utc::now().timestamp();
    let exp = now + ttl_secs.unwrap_or(DEFAULT_CAPABILITY_TTL_SECS);

    let mut nonce_bytes = [0u8; 16];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = hex::encode(nonce_bytes);

    let claims = CapabilityClaims {
        iss: signer.issuer().to_owned(),
        sub: principal_id.to_owned(),
        iat: now,
        exp,
        conversation_id: conversation_id.to_owned(),
        turn_seq,
        capability_set,
        nonce,
    };

    let header = Header::new(Algorithm::EdDSA);
    // Reuse the pre-parsed encoding key from the signer instead of
    // re-decoding PEM bytes per call (§0 axiom #14 — saves ~25µs).
    encode(&header, &claims, signer.encoding_key()).expect("JWT encode")
}

/// Verify a capability token. Returns the decoded claims if the token
/// is valid, signed by us, AND the turn_seq matches the expected
/// `current_turn_seq` (mismatch ⇒ reject per §7.2).
pub fn verify_capability_token(
    signer: &JwtSigner,
    token: &str,
    expected_conversation_id: &str,
    expected_turn_seq: i64,
) -> Result<CapabilityClaims, String> {
    use jsonwebtoken::{Algorithm, Validation, decode};

    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_issuer(&[signer.issuer()]);
    validation.leeway = 5;

    // Reuse the cached decoding key (§0 axiom #14 — saves ~25µs).
    let data = decode::<CapabilityClaims>(token, signer.decoding_key(), &validation)
        .map_err(|e| format!("token verification failed: {e}"))?;

    if data.claims.conversation_id != expected_conversation_id {
        return Err(format!(
            "capability token is for conversation '{}', not '{}'",
            data.claims.conversation_id, expected_conversation_id
        ));
    }
    if data.claims.turn_seq != expected_turn_seq {
        return Err(format!(
            "capability token is for turn {}, but current turn is {}",
            data.claims.turn_seq, expected_turn_seq
        ));
    }
    Ok(data.claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_token_verifies() {
        let signer = JwtSigner::generate("execlaw-test".into());
        let token = issue_capability_token(
            &signer,
            "pri_ctrl",
            "conv_abc",
            47,
            vec!["tools.safe".into()],
            None,
        );
        let claims = verify_capability_token(&signer, &token, "conv_abc", 47).unwrap();
        assert_eq!(claims.conversation_id, "conv_abc");
        assert_eq!(claims.turn_seq, 47);
        assert_eq!(claims.sub, "pri_ctrl");
        assert_eq!(claims.capability_set, vec!["tools.safe"]);
    }

    #[test]
    fn wrong_turn_seq_rejected() {
        let signer = JwtSigner::generate("execlaw-test".into());
        let token = issue_capability_token(
            &signer,
            "pri_ctrl",
            "conv_abc",
            47,
            vec!["tools.safe".into()],
            None,
        );
        let err = verify_capability_token(&signer, &token, "conv_abc", 48).unwrap_err();
        assert!(err.contains("for turn 47"));
    }

    #[test]
    fn wrong_conversation_rejected() {
        let signer = JwtSigner::generate("execlaw-test".into());
        let token = issue_capability_token(
            &signer,
            "pri_ctrl",
            "conv_abc",
            47,
            vec!["tools.safe".into()],
            None,
        );
        let err = verify_capability_token(&signer, &token, "conv_xyz", 47).unwrap_err();
        assert!(err.contains("for conversation"));
    }

    #[test]
    fn different_signer_cannot_verify() {
        let signer_a = JwtSigner::generate("execlaw-test".into());
        let signer_b = JwtSigner::generate("execlaw-test".into());
        let token = issue_capability_token(&signer_a, "pri_ctrl", "conv_abc", 1, vec![], None);
        let err = verify_capability_token(&signer_b, &token, "conv_abc", 1).unwrap_err();
        assert!(err.contains("verification failed"));
    }

    #[test]
    fn nonce_differs_per_issuance() {
        let signer = JwtSigner::generate("execlaw-test".into());
        let t1 = issue_capability_token(&signer, "p", "c", 1, vec![], None);
        let t2 = issue_capability_token(&signer, "p", "c", 1, vec![], None);
        assert_ne!(t1, t2, "nonce should make every token distinct");
    }

    /// Expired capability tokens must not verify. The jsonwebtoken
    /// validator gives us 5 seconds of leeway, so we back-date by 300s
    /// to guarantee rejection.
    #[test]
    fn expired_token_rejected() {
        let signer = JwtSigner::generate("execlaw-test".into());
        let token = issue_capability_token(
            &signer,
            "pri_ctrl",
            "conv_abc",
            1,
            vec!["tools.safe".into()],
            Some(-300), // 5 minutes in the past
        );
        let err = verify_capability_token(&signer, &token, "conv_abc", 1).unwrap_err();
        assert!(err.to_lowercase().contains("expired") || err.contains("verification failed"));
    }

    /// Flipping one byte of the JWT's payload segment must invalidate
    /// the signature. This is the core tamper-detection property.
    #[test]
    fn tampered_payload_segment_rejected() {
        let signer = JwtSigner::generate("execlaw-test".into());
        let token = issue_capability_token(
            &signer,
            "pri_ctrl",
            "conv_abc",
            47,
            vec!["tools.safe".into()],
            None,
        );
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT should have header.payload.signature");
        // Flip last char of the payload segment — Base64URL safe chars
        // swap to produce a different but valid base64 char.
        let mut payload = parts[1].to_owned();
        let last = payload.pop().unwrap();
        let replacement = if last == 'A' { 'B' } else { 'A' };
        payload.push(replacement);
        let tampered = format!("{}.{}.{}", parts[0], payload, parts[2]);
        let err = verify_capability_token(&signer, &tampered, "conv_abc", 47).unwrap_err();
        assert!(err.contains("verification failed") || err.contains("decode"));
    }

    /// Truncating the signature segment must be rejected.
    #[test]
    fn truncated_signature_rejected() {
        let signer = JwtSigner::generate("execlaw-test".into());
        let token = issue_capability_token(&signer, "p", "c", 1, vec![], None);
        let parts: Vec<&str> = token.split('.').collect();
        let truncated = format!("{}.{}.", parts[0], parts[1]);
        let err = verify_capability_token(&signer, &truncated, "c", 1).unwrap_err();
        assert!(!err.is_empty());
    }
}
