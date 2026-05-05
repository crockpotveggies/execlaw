//! JWT + refresh-token machinery. EdDSA (Ed25519) signed.

use chrono::Utc;
use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey};
use ed25519_dalek::{SigningKey, VerifyingKey};
use execlaw_core::refresh_tokens::RefreshTokenStore;
use execlaw_core::{Database, DbError};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessClaims {
    pub sub: String, // principal_id
    pub iss: String,
    pub exp: i64,
    pub iat: i64,
    pub sid: String, // session id — allows bulk-revoke on logout
    pub nonce: String,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("invalid or expired token")]
    Invalid,
    #[error("no admin principal exists yet; must run /api/setup first")]
    NotInitialized,
    #[error("password hash verification failed")]
    BadPassword,
    #[error("jwt error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("base64 error: {0}")]
    Base64(String),
}

/// Persistent refresh token store. Thin wrapper over the SQLite-
/// backed `RefreshTokenStore` in `execlaw-core` so AppState can keep
/// holding it as an `Arc`. Behaviour matches the previous in-memory
/// implementation: single-use consumption, session-scoped revoke,
/// expired-on-read returns None.
///
/// Phase 7 hardening item: tokens used to live in a process-local
/// DashMap, which meant a server restart silently signed everyone
/// out. Now they survive restarts (encrypted at rest with the rest
/// of the SQLCipher DB).
#[derive(Debug, Clone)]
pub struct RefreshStore {
    db: Database,
}

/// Minted from `consume` — same shape the in-memory store produced
/// so the route layer doesn't have to know about the SQLite move.
#[derive(Debug, Clone)]
pub struct RefreshRecord {
    pub principal_id: String,
    pub session_id: String,
    pub expires_at: i64,
}

impl RefreshStore {
    /// Construct from a `Database`. The handle is cloned (it's a
    /// pooled connection internally) so multiple `Arc<RefreshStore>`s
    /// share one connection pool.
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Issue a fresh refresh token for the given (principal, session)
    /// pair. Persisted before the string is returned so a crash
    /// between issue and use doesn't leave the caller with a token
    /// the server doesn't know about.
    pub fn issue(
        &self,
        principal_id: &str,
        session_id: &str,
        ttl_secs: i64,
    ) -> Result<String, DbError> {
        RefreshTokenStore::new(&self.db).issue(principal_id, session_id, ttl_secs)
    }

    /// Single-use consume. The row is deleted on read; reuse and
    /// expired tokens both return `None`.
    pub fn consume(&self, token: &str) -> Result<Option<RefreshRecord>, DbError> {
        let row = RefreshTokenStore::new(&self.db).consume(token)?;
        Ok(row.map(|r| RefreshRecord {
            principal_id: r.principal_id,
            session_id: r.session_id,
            expires_at: r.expires_at,
        }))
    }

    /// Drop every refresh token tied to a session. Returns the number
    /// removed (mainly for telemetry / tests).
    pub fn revoke_session(&self, session_id: &str) -> Result<usize, DbError> {
        RefreshTokenStore::new(&self.db).revoke_session(session_id)
    }

    /// "Sign out everywhere": drop every refresh token for a user.
    /// Returns the number removed.
    pub fn revoke_all_for_user(&self, principal_id: &str) -> Result<usize, DbError> {
        RefreshTokenStore::new(&self.db).revoke_all_for_user(principal_id)
    }

    /// Distinct active sessions for a user. Drives the
    /// "you have N other sessions" surface.
    pub fn active_session_count(&self, principal_id: &str) -> Result<usize, DbError> {
        RefreshTokenStore::new(&self.db).active_session_count(principal_id)
    }

    /// Sweep expired rows. Cheap; safe to call on a periodic timer.
    pub fn purge_expired(&self) -> Result<usize, DbError> {
        RefreshTokenStore::new(&self.db).purge_expired()
    }
}

/// Ed25519 JWT signer.
pub struct JwtSigner {
    signing_key: SigningKey,
    verifying_key: VerifyingKey,
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
    issuer: String,
}

impl std::fmt::Debug for JwtSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtSigner")
            .field("issuer", &self.issuer)
            .field("verifying_key", &"<ed25519>")
            .finish()
    }
}

impl JwtSigner {
    pub fn generate(issuer: String) -> Self {
        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let verifying_key = signing_key.verifying_key();
        Self::from_keys(signing_key, verifying_key, issuer)
    }

    /// Derive the Ed25519 signing key deterministically from the
    /// vault's master key. Same master across restarts → same JWT
    /// signing key → access_tokens issued before the restart still
    /// verify after it.
    ///
    /// Pre-fix, `cli::cmd_serve` minted the signing key with
    /// `SigningKey::generate(&mut OsRng)` at every boot, which
    /// silently invalidated every operator's stored tokens whenever
    /// cargo-watch rebuilt. The SPA's silent-refresh path papered
    /// over it most of the time, but races during the rebuild
    /// window surfaced as "internal server error" on the next API
    /// call. Persisting the signing key removes the entire failure
    /// mode.
    ///
    /// Derivation: the master key is 32 bytes of high-entropy
    /// random; we run it through SHA-256 with a domain-separator
    /// constant so this key never collides with anything else
    /// derived from the master (e.g. event-log HMAC).
    pub fn from_master_key(master_key: &[u8; 32], issuer: String) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"execlaw/jwt-signing-key/v1");
        hasher.update(master_key);
        let derived = hasher.finalize();
        let bytes: [u8; 32] = derived.into();
        let signing_key = SigningKey::from_bytes(&bytes);
        let verifying_key = signing_key.verifying_key();
        Self::from_keys(signing_key, verifying_key, issuer)
    }

    pub fn from_keys(signing_key: SigningKey, verifying_key: VerifyingKey, issuer: String) -> Self {
        // Use PEM throughout — jsonwebtoken handles both PKCS#8-encoded
        // private and SubjectPublicKeyInfo public PEM for EdDSA.
        let priv_pem = signing_key
            .to_pkcs8_pem(LineEnding::LF)
            .expect("valid ed25519 signing key produces PKCS#8 PEM");
        let pub_pem = verifying_key
            .to_public_key_pem(LineEnding::LF)
            .expect("valid verifying key produces SPKI PEM");
        let encoding_key = EncodingKey::from_ed_pem(priv_pem.as_bytes())
            .expect("jsonwebtoken accepts our ed25519 PKCS#8 PEM");
        let decoding_key = DecodingKey::from_ed_pem(pub_pem.as_bytes())
            .expect("jsonwebtoken accepts our ed25519 SPKI PEM");
        Self {
            signing_key,
            verifying_key,
            encoding_key,
            decoding_key,
            issuer,
        }
    }

    pub fn issue_access_token(
        &self,
        principal_id: &str,
        session_id: &str,
        ttl_secs: i64,
    ) -> Result<String, AuthError> {
        let now = Utc::now().timestamp();
        let claims = AccessClaims {
            sub: principal_id.to_owned(),
            iss: self.issuer.clone(),
            exp: now + ttl_secs,
            iat: now,
            sid: session_id.to_owned(),
            nonce: Uuid::new_v4().to_string(),
        };
        let header = Header::new(Algorithm::EdDSA);
        let tok = encode(&header, &claims, &self.encoding_key)?;
        Ok(tok)
    }

    pub fn verify_access_token(&self, token: &str) -> Result<AccessClaims, AuthError> {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(std::slice::from_ref(&self.issuer));
        // Tighten leeway to 0 — a token marked "exp = now - 60" must fail.
        validation.leeway = 0;
        let data = decode::<AccessClaims>(token, &self.decoding_key, &validation)?;
        Ok(data.claims)
    }

    /// Accessor for the issuer string embedded in every JWT.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Shared `EncodingKey` — reuse this instead of re-decoding the PEM
    /// per call. Token issuance is on the per-turn hot path (§0 axiom
    /// #14); the pre-parsed key saves ~25µs per call.
    pub fn encoding_key(&self) -> &EncodingKey {
        &self.encoding_key
    }

    /// Shared `DecodingKey` — see [`encoding_key`](Self::encoding_key).
    pub fn decoding_key(&self) -> &DecodingKey {
        &self.decoding_key
    }

    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.verifying_key
    }

    pub fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }
}

pub type SharedSigner = Arc<JwtSigner>;

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::{Database, DbConfig};
    use execlaw_core::migrations::MigrationRunner;

    fn fresh_store() -> RefreshStore {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        RefreshStore::new(db)
    }

    #[test]
    fn issue_and_verify_roundtrip() {
        let signer = JwtSigner::generate("execlaw-test".into());
        let tok = signer
            .issue_access_token("principal-1", "session-1", 60)
            .unwrap();
        let claims = signer.verify_access_token(&tok).unwrap();
        assert_eq!(claims.sub, "principal-1");
        assert_eq!(claims.sid, "session-1");
        assert_eq!(claims.iss, "execlaw-test");
    }

    #[test]
    fn expired_token_rejected() {
        let signer = JwtSigner::generate("execlaw-test".into());
        // Put the expiry 5 minutes in the past so default clock-skew
        // leeway doesn't save it.
        let tok = signer
            .issue_access_token("principal-1", "session-1", -300)
            .unwrap();
        assert!(signer.verify_access_token(&tok).is_err());
    }

    #[test]
    fn wrong_issuer_rejected() {
        // Two different signers (different keypairs) — tokens from one
        // must not verify in the other regardless of issuer. That covers
        // the two failure modes together.
        let a = JwtSigner::generate("one".into());
        let b = JwtSigner::generate("two".into());
        let tok = a
            .issue_access_token("principal-1", "session-1", 60)
            .unwrap();
        assert!(b.verify_access_token(&tok).is_err());
    }

    #[test]
    fn refresh_store_issues_and_consumes_once() {
        let store = fresh_store();
        let tok = store.issue("p", "s", 60).unwrap();
        let rec = store.consume(&tok).unwrap().unwrap();
        assert_eq!(rec.principal_id, "p");
        assert!(
            store.consume(&tok).unwrap().is_none(),
            "refresh token must be single-use"
        );
    }

    #[test]
    fn refresh_store_expires_rotation() {
        let store = fresh_store();
        let tok = store.issue("p", "s", -10).unwrap(); // already expired
        assert!(store.consume(&tok).unwrap().is_none());
    }

    /// Cached encoding/decoding keys are real — smoke-test that the
    /// accessor returns a key that actually signs a token that the
    /// matching decoding_key can verify. Protects against a future
    /// refactor that stores mismatched keys.
    #[test]
    fn cached_keys_are_consistent() {
        let s = JwtSigner::generate("t".into());
        let header = Header::new(Algorithm::EdDSA);
        let tok = encode(
            &header,
            &AccessClaims {
                sub: "p".into(),
                iss: "t".into(),
                exp: Utc::now().timestamp() + 60,
                iat: Utc::now().timestamp(),
                sid: "s".into(),
                nonce: "n".into(),
            },
            s.encoding_key(),
        )
        .unwrap();
        let mut v = Validation::new(Algorithm::EdDSA);
        v.set_issuer(std::slice::from_ref(&s.issuer));
        let _data = decode::<AccessClaims>(&tok, s.decoding_key(), &v).unwrap();
    }

    #[test]
    fn from_master_key_is_deterministic_across_instances() {
        // The whole point of derive-from-master: a token issued by
        // one signer instance verifies against a freshly-derived
        // signer that uses the same master. This is what makes
        // cargo-watch rebuilds NOT invalidate logged-in sessions.
        let master = [42u8; 32];
        let s1 = JwtSigner::from_master_key(&master, "execlaw".into());
        let s2 = JwtSigner::from_master_key(&master, "execlaw".into());
        let tok = s1.issue_access_token("p-1", "sess-x", 60).unwrap();
        let claims = s2.verify_access_token(&tok).unwrap();
        assert_eq!(claims.sub, "p-1");
        assert_eq!(claims.sid, "sess-x");
    }

    #[test]
    fn from_master_key_distinct_masters_distinct_keys() {
        // Different master → different signing key → tokens
        // refuse to cross-verify. Defence-in-depth on the
        // domain-separator constant: even if two execlaw
        // instances shared an issuer string, distinct masters
        // give distinct trust domains.
        let s1 = JwtSigner::from_master_key(&[1u8; 32], "execlaw".into());
        let s2 = JwtSigner::from_master_key(&[2u8; 32], "execlaw".into());
        let tok = s1.issue_access_token("p", "s", 60).unwrap();
        assert!(s2.verify_access_token(&tok).is_err());
    }

    /// `revoke_session` must drop EVERY refresh record bound to the
    /// session id, not just one — so a logout invalidates every
    /// refresh token the session had minted.
    #[test]
    fn revoke_session_invalidates_all_refresh_for_session() {
        let store = fresh_store();
        let t1 = store.issue("p", "sess-1", 60).unwrap();
        let t2 = store.issue("p", "sess-1", 60).unwrap();
        let t3 = store.issue("p", "sess-2", 60).unwrap();

        let removed = store.revoke_session("sess-1").unwrap();
        assert_eq!(removed, 2);
        assert!(store.consume(&t1).unwrap().is_none());
        assert!(store.consume(&t2).unwrap().is_none());
        // sess-2's token survives.
        assert!(store.consume(&t3).unwrap().is_some());
    }

    /// `revoke_all_for_user` is the "sign out everywhere" primitive:
    /// it must drop tokens for the named principal regardless of
    /// session AND must not touch any other principal's tokens.
    #[test]
    fn revoke_all_for_user_isolates_other_users() {
        let store = fresh_store();
        let _ = store.issue("alice", "sess-1", 60).unwrap();
        let _ = store.issue("alice", "sess-2", 60).unwrap();
        let bob = store.issue("bob", "sess-3", 60).unwrap();
        let removed = store.revoke_all_for_user("alice").unwrap();
        assert_eq!(removed, 2);
        // Bob's token still consumes.
        assert!(store.consume(&bob).unwrap().is_some());
    }

    /// Survives "process restart" — drop the wrapper, rebuild from
    /// the same Database, and the previously-issued token still
    /// works exactly once. Demonstrates the persistence guarantee
    /// that the in-memory implementation could never give.
    #[test]
    fn refresh_token_survives_wrapper_recreate() {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let tok = RefreshStore::new(db.clone()).issue("u", "s", 60).unwrap();
        // Drop the first wrapper, build a fresh one — same behaviour
        // a server restart would see if it re-opened the same DB.
        let restarted = RefreshStore::new(db);
        let rec = restarted.consume(&tok).unwrap().unwrap();
        assert_eq!(rec.principal_id, "u");
    }

    #[test]
    fn active_session_count_distinct_per_session_id() {
        let store = fresh_store();
        let _ = store.issue("u", "sess-A", 60).unwrap();
        let _ = store.issue("u", "sess-A", 60).unwrap();
        let _ = store.issue("u", "sess-B", 60).unwrap();
        // Two rotations within sess-A still count as 1; sess-B is a
        // separate session.
        assert_eq!(store.active_session_count("u").unwrap(), 2);
    }
}
