//! HMAC-signed event-log records (§7.8 security-defense port from
//! `selfhosted-claw/src/control-store.ts`).
//!
//! Every `state_events` row gets an HMAC-SHA256 tag computed over its
//! canonical bytes. Tampering with any committed row — changing the
//! `kind`, the `payload`, the `actor`, the `committed_at` — invalidates
//! the tag, so post-hoc event-log modification is detectable at replay.
//!
//! The HMAC key lives in the vault (`vault_secrets` table, key name
//! `event_log_hmac_key`). Rotating the key re-signs nothing — old
//! events remain verifiable under the key in effect when they were
//! written; we track which key version signed each row via a small
//! key-id column added by a later migration. For Phase 1, we ship
//! a single-key scheme; key rotation is a Phase 2 hardening.
//!
//! **No cloud dependencies.** `hmac` + `sha2` crates, pure Rust.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Canonical bytes that go into the HMAC. Order must be stable across
/// versions — any change invalidates existing tags. The format is:
///
/// ```text
/// conversation_id || 0 || seq (little-endian i64) ||
///   kind || 0 || committed_at (little-endian i64) ||
///   actor_or_empty || 0 || payload_bytes
/// ```
///
/// `\0` separators prevent field-smuggling (an attacker re-arranging
/// bytes across fields can't produce a colliding canonical encoding).
pub fn canonical_bytes(
    conversation_id: &str,
    seq: i64,
    kind: &str,
    committed_at: i64,
    actor: Option<&str>,
    payload: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(conversation_id.len() + kind.len() + payload.len() + 32);
    buf.extend_from_slice(conversation_id.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.extend_from_slice(kind.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&committed_at.to_le_bytes());
    buf.extend_from_slice(actor.unwrap_or("").as_bytes());
    buf.push(0);
    buf.extend_from_slice(payload);
    buf
}

/// Compute the HMAC-SHA256 tag for an event row.
pub fn sign_event(key: &[u8], canonical: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(canonical);
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

/// Verify that `tag` is the correct HMAC for `canonical` under `key`.
/// Constant-time comparison via the `hmac` crate internals.
pub fn verify_event(key: &[u8], canonical: &[u8], tag: &[u8; 32]) -> bool {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(canonical);
    mac.verify_slice(tag).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_signs_and_verifies() {
        let key = b"test-hmac-key-32-bytes-long!!!!!";
        let canon = canonical_bytes(
            "conv-1",
            7,
            "user_msg",
            1_714_000_000,
            Some("pri_ctrl"),
            b"msgpack-payload-bytes",
        );
        let tag = sign_event(key, &canon);
        assert!(verify_event(key, &canon, &tag));
    }

    #[test]
    fn tampered_payload_fails_verification() {
        let key = b"k";
        let canon = canonical_bytes("c", 1, "user_msg", 0, None, b"original");
        let tag = sign_event(key, &canon);
        let tampered = canonical_bytes("c", 1, "user_msg", 0, None, b"modified");
        assert!(!verify_event(key, &tampered, &tag));
    }

    #[test]
    fn tampered_seq_fails_verification() {
        let key = b"k";
        let canon = canonical_bytes("c", 1, "user_msg", 0, None, b"p");
        let tag = sign_event(key, &canon);
        let shifted = canonical_bytes("c", 2, "user_msg", 0, None, b"p");
        assert!(!verify_event(key, &shifted, &tag));
    }

    #[test]
    fn different_key_fails_verification() {
        let canon = canonical_bytes("c", 1, "user_msg", 0, None, b"p");
        let tag = sign_event(b"key-a", &canon);
        assert!(!verify_event(b"key-b", &canon, &tag));
    }

    #[test]
    fn null_separator_prevents_field_smuggling() {
        // Ensure that two different fields can't produce the same
        // canonical bytes by shuffling content across the boundary.
        let a = canonical_bytes("foo", 1, "bar", 0, None, b"");
        let b = canonical_bytes("foob", 1, "ar", 0, None, b"");
        assert_ne!(a, b);
    }
}
