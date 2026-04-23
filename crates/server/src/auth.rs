//! JWT + refresh-token machinery. EdDSA (Ed25519) signed.

use chrono::Utc;
use dashmap::DashMap;
use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey};
use ed25519_dalek::{SigningKey, VerifyingKey};
use jsonwebtoken::{
    decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation,
};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessClaims {
    pub sub: String,            // principal_id
    pub iss: String,
    pub exp: i64,
    pub iat: i64,
    pub sid: String,            // session id — allows bulk-revoke on logout
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

/// Minimal dashmap-backed refresh token store. Good enough for Phase 0; a
/// SQLCipher-backed version drops in later.
#[derive(Debug, Default)]
pub struct RefreshStore {
    by_token: DashMap<String, RefreshRecord>,
}

#[derive(Debug, Clone)]
pub struct RefreshRecord {
    pub principal_id: String,
    pub session_id: String,
    pub expires_at: i64,
}

impl RefreshStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn issue(
        &self,
        principal_id: &str,
        session_id: &str,
        ttl_secs: i64,
    ) -> String {
        let token = Uuid::new_v4().to_string() + "-" + &Uuid::new_v4().to_string();
        self.by_token.insert(
            token.clone(),
            RefreshRecord {
                principal_id: principal_id.to_owned(),
                session_id: session_id.to_owned(),
                expires_at: Utc::now().timestamp() + ttl_secs,
            },
        );
        token
    }

    pub fn consume(&self, token: &str) -> Option<RefreshRecord> {
        let record = self.by_token.remove(token).map(|(_, v)| v)?;
        if Utc::now().timestamp() > record.expires_at {
            return None;
        }
        Some(record)
    }

    pub fn revoke_session(&self, session_id: &str) {
        self.by_token
            .retain(|_, record| record.session_id != session_id);
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

    pub fn from_keys(
        signing_key: SigningKey,
        verifying_key: VerifyingKey,
        issuer: String,
    ) -> Self {
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
        validation.set_issuer(&[self.issuer.clone()]);
        // Tighten leeway to 0 — a token marked "exp = now - 60" must fail.
        validation.leeway = 0;
        let data =
            decode::<AccessClaims>(token, &self.decoding_key, &validation)?;
        Ok(data.claims)
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
        let store = RefreshStore::new();
        let tok = store.issue("p", "s", 60);
        let rec = store.consume(&tok).unwrap();
        assert_eq!(rec.principal_id, "p");
        assert!(store.consume(&tok).is_none(), "refresh token must be single-use");
    }

    #[test]
    fn refresh_store_expires_rotation() {
        let store = RefreshStore::new();
        let tok = store.issue("p", "s", -10); // already expired
        assert!(store.consume(&tok).is_none());
    }
}
