//! Signed download URLs for browser-direct GETs.
//!
//! Replaces the pre-2026-05 `?access_token=<jwt>` query-string
//! fallback on the `AuthedUser` extractor. That fallback let the
//! operator's full-access JWT travel through browser history,
//! referrers, proxy logs, and copied-link surfaces — and a JWT is
//! a wildcard credential that authorises every API call. The
//! audit flagged this as broad leak surface; this module is the
//! fix.
//!
//! ## URL shape
//!
//! `<path>?exp=<unix_seconds>&user=<user_id>&sig=<hex>`
//!
//! `sig` is `HMAC-SHA256(download_hmac_key, "execlaw/download-url/v1\n"
//! || path || "\n" || user_id || "\n" || exp)` rendered as hex.
//!
//! The HMAC binds the URL to a specific path AND user AND expiry.
//! Tampering with any of those three breaks the signature; the
//! signed URL grants no authority beyond "GET this exact path as
//! this user before exp."
//!
//! Default TTL: 5 minutes (`DEFAULT_TTL_SECS`). Hard cap: 1 hour.
//! Short windows are fine in practice because the SPA can re-sign
//! on demand (every render of an attachment chip / image src
//! triggers a fresh sign via the `/api/downloads/sign` endpoint).
//!
//! ## Extractor
//!
//! `MediaAuthedUser` is the alternative to `AuthedUser` for routes
//! that the browser hits directly (`<a download>`, `<img src>`,
//! video / audio src). It tries the Authorization header first;
//! if that fails, it falls back to verifying the signed URL.
//! Routes that only get fetch-driven traffic should keep using
//! `AuthedUser` so they reject query-token attempts.

use crate::auth::AuthError;
use crate::auth_extract::{AuthRejection, AuthedUser};
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::Utc;
use execlaw_core::users::UserStore;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::BTreeMap;

/// Default TTL for a freshly signed URL. 5 minutes is long enough
/// for the operator to right-click → save-as without re-renders
/// invalidating the link mid-flight, and short enough that a URL
/// posted to a chat / pasted into Slack expires before anyone
/// downstream gets meaningful use of it.
pub const DEFAULT_TTL_SECS: i64 = 5 * 60;

/// Hard upper bound. Lets an operator request a longer window
/// (e.g. for very large downloads on slow connections) without
/// letting them mint hour-long credentials.
pub const MAX_TTL_SECS: i64 = 60 * 60;

/// Compute the hex-encoded HMAC for a `(path, user_id, exp)` tuple.
/// Stable across versions — the leading domain-separator
/// `"execlaw/download-url/v1\n"` will be bumped if the canonicalisation
/// ever changes, so old signatures cleanly stop verifying.
pub fn compute_sig(path: &str, user_id: &str, exp_unix: i64, key: &[u8]) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key)
        .expect("Hmac::new_from_slice accepts any byte slice");
    mac.update(b"execlaw/download-url/v1\n");
    mac.update(path.as_bytes());
    mac.update(b"\n");
    mac.update(user_id.as_bytes());
    mac.update(b"\n");
    mac.update(exp_unix.to_string().as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Build a signed URL for `path`. `ttl_secs` is clamped to
/// `[1, MAX_TTL_SECS]`. Returns the URL string and the absolute
/// `exp` unix-seconds so the caller can surface a "valid until …"
/// hint to the operator.
pub fn build_signed_url(
    path: &str,
    user_id: &str,
    ttl_secs: i64,
    key: &[u8],
) -> (String, i64) {
    let ttl = ttl_secs.clamp(1, MAX_TTL_SECS);
    let exp = Utc::now().timestamp() + ttl;
    let sig = compute_sig(path, user_id, exp, key);
    let url = format!(
        "{path}?exp={exp}&user={user}&sig={sig}",
        user = percent_encode(user_id),
    );
    (url, exp)
}

/// Constant-time byte-slice equality.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Verify the signature on a request. Caller already parsed
/// `path`, `query`, and the key from the request — this fn is the
/// pure crypto step.
pub fn verify_sig(
    path: &str,
    query: &str,
    key: &[u8],
) -> Result<VerifiedDownloadClaims, SignedUrlError> {
    let params = parse_query(query);
    let exp_str = params
        .get("exp")
        .ok_or(SignedUrlError::MissingParam("exp"))?;
    let user = params
        .get("user")
        .ok_or(SignedUrlError::MissingParam("user"))?;
    let sig_hex = params
        .get("sig")
        .ok_or(SignedUrlError::MissingParam("sig"))?;
    let exp: i64 = exp_str.parse().map_err(|_| SignedUrlError::BadExp)?;
    if exp < Utc::now().timestamp() {
        return Err(SignedUrlError::Expired);
    }
    let expected = compute_sig(path, user, exp, key);
    if !ct_eq(expected.as_bytes(), sig_hex.as_bytes()) {
        return Err(SignedUrlError::BadSig);
    }
    Ok(VerifiedDownloadClaims {
        user_id: user.clone(),
        exp,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDownloadClaims {
    pub user_id: String,
    pub exp: i64,
}

/// Errors from [`verify_sig`]. Mapped to a single 401 at the
/// extractor boundary so a probing caller can't distinguish
/// expired-vs-tampered-vs-malformed.
#[derive(Debug, PartialEq, Eq)]
pub enum SignedUrlError {
    MissingParam(&'static str),
    BadExp,
    Expired,
    BadSig,
}

/// Hand-rolled query-string parser. Avoids pulling in
/// `url::form_urlencoded` for one lookup; handles `key=val&key2=val2`,
/// percent-encoded `+`, and `%XX`. Identical algorithm to the helper
/// `auth_extract.rs` used to ship.
fn parse_query(q: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        let Some(k) = it.next() else { continue };
        if k.is_empty() {
            continue;
        }
        let v = it.next().unwrap_or("");
        out.insert(percent_decode(k), percent_decode(v));
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_nibble(bytes[i + 1]);
                let lo = hex_nibble(bytes[i + 2]);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h << 4) | l);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Tight percent-encode for the `user` query value: encode anything
/// outside the unreserved alphanumeric set. The user_id is typically
/// a UUID or short ASCII slug, so almost always passes through
/// unchanged; the encoding handles the edge case (and matches the
/// SPA's `encodeURIComponent`).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Wrapper around `AuthedUser` for routes the browser hits directly
/// (link navigations, `<img src>`, video/audio src). Accepts either:
///   * Authorization: Bearer <jwt> — for fetch-driven calls; or
///   * the `?exp&user&sig` signed-URL trio — for browser-direct GETs.
///
/// Implementing fn boundaries: this extractor takes `&mut Parts`,
/// runs `AuthedUser::from_request_parts` first (which only inspects
/// headers in its post-2026-05 form), then on rejection falls back to
/// signed-URL verification. The returned `AuthedUser` value is the
/// same shape regardless of which path succeeded — handler code
/// doesn't branch.
pub struct MediaAuthedUser(pub AuthedUser);

impl FromRequestParts<AppState> for MediaAuthedUser {
    type Rejection = AuthRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Header-based auth first — preserves the existing
        // fetch-driven call path with no behavior change.
        if parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .is_some()
        {
            return AuthedUser::from_request_parts(parts, state)
                .await
                .map(MediaAuthedUser);
        }
        // Signed-URL fallback.
        let path = parts.uri.path().to_owned();
        let query = parts.uri.query().unwrap_or_default().to_owned();
        let claims = verify_sig(&path, &query, state.signer.download_hmac_key())
            .map_err(|_| AuthRejection("signed URL invalid or expired"))?;
        let users = UserStore::new(&state.db);
        let row = users
            .get_by_id(&claims.user_id)
            .map_err(|_| AuthRejection("user lookup failed"))?
            .ok_or(AuthRejection(
                "signed URL references a user that no longer exists",
            ))?;
        Ok(MediaAuthedUser(AuthedUser::from(row)))
    }
}

/// For the rare caller that wants to verify a signed URL outside
/// the extractor flow (admin probes, tests). Returns the underlying
/// crypto error variant rather than collapsing to 401.
pub fn verify_request(
    path: &str,
    query: &str,
    state: &AppState,
) -> Result<VerifiedDownloadClaims, SignedUrlError> {
    verify_sig(path, query, state.signer.download_hmac_key())
}

// `AuthError` is re-imported for clarity in the signature above —
// silencing unused-import lint without exposing it as part of the
// public API.
#[allow(dead_code)]
type _AuthErrorReexport = AuthError;

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"test-download-hmac-key-32-bytes-x";

    #[test]
    fn build_then_verify_roundtrips() {
        let (url, exp) = build_signed_url("/api/attachments/abc", "u-1", 60, KEY);
        // URL must contain all three params.
        assert!(url.starts_with("/api/attachments/abc?"));
        assert!(url.contains(&format!("exp={exp}")));
        assert!(url.contains("user=u-1"));
        assert!(url.contains("sig="));
        let query = url.split_once('?').unwrap().1;
        let claims = verify_sig("/api/attachments/abc", query, KEY).unwrap();
        assert_eq!(claims.user_id, "u-1");
        assert_eq!(claims.exp, exp);
    }

    #[test]
    fn tampered_path_rejected() {
        let (url, _) = build_signed_url("/api/attachments/abc", "u-1", 60, KEY);
        let query = url.split_once('?').unwrap().1;
        // Verify against a DIFFERENT path — sig binding catches it.
        assert_eq!(
            verify_sig("/api/attachments/SOMETHING-ELSE", query, KEY),
            Err(SignedUrlError::BadSig)
        );
    }

    #[test]
    fn tampered_user_rejected() {
        let (url, _) = build_signed_url("/api/attachments/abc", "u-1", 60, KEY);
        // Swap user but keep sig.
        let q = url.split_once('?').unwrap().1.replace("user=u-1", "user=u-2");
        assert_eq!(
            verify_sig("/api/attachments/abc", &q, KEY),
            Err(SignedUrlError::BadSig)
        );
    }

    #[test]
    fn tampered_sig_rejected() {
        let (url, _) = build_signed_url("/api/attachments/abc", "u-1", 60, KEY);
        // Flip a hex character in sig.
        let q = url.split_once('?').unwrap().1;
        let bad = q.replace("sig=", "sig=ff");
        assert_eq!(
            verify_sig("/api/attachments/abc", &bad, KEY),
            Err(SignedUrlError::BadSig)
        );
    }

    #[test]
    fn expired_url_rejected() {
        let exp = Utc::now().timestamp() - 1;
        let sig = compute_sig("/x", "u", exp, KEY);
        let q = format!("exp={exp}&user=u&sig={sig}");
        assert_eq!(verify_sig("/x", &q, KEY), Err(SignedUrlError::Expired));
    }

    #[test]
    fn missing_param_rejected() {
        assert_eq!(
            verify_sig("/x", "exp=1&user=u", KEY),
            Err(SignedUrlError::MissingParam("sig"))
        );
        assert_eq!(
            verify_sig("/x", "user=u&sig=00", KEY),
            Err(SignedUrlError::MissingParam("exp"))
        );
        assert_eq!(
            verify_sig("/x", "exp=1&sig=00", KEY),
            Err(SignedUrlError::MissingParam("user"))
        );
    }

    #[test]
    fn wrong_key_rejected() {
        let (url, _) = build_signed_url("/x", "u", 60, KEY);
        let q = url.split_once('?').unwrap().1;
        let wrong_key = b"different-key-different-different".as_slice();
        assert_eq!(verify_sig("/x", q, wrong_key), Err(SignedUrlError::BadSig));
    }

    #[test]
    fn ttl_clamped_to_max() {
        let (_, exp1) = build_signed_url("/x", "u", MAX_TTL_SECS + 10_000, KEY);
        let (_, exp2) = build_signed_url("/x", "u", MAX_TTL_SECS, KEY);
        // Both should land at roughly the same `now + MAX_TTL_SECS`.
        // Allow a 2-second slack for clock-tick between calls.
        assert!((exp1 - exp2).abs() <= 2, "exp1={exp1} exp2={exp2}");
    }

    #[test]
    fn percent_encoded_user_id_roundtrips() {
        // user_ids in execlaw are typically `u-<uuid>` or
        // `pri_<slug>` — pure alphanumeric+dashes. But the encoding
        // must round-trip arbitrary chars in case a future identity
        // shape carries one.
        let weird = "user with spaces & slashes/here";
        let (url, _) = build_signed_url("/x", weird, 60, KEY);
        let q = url.split_once('?').unwrap().1;
        let claims = verify_sig("/x", &q, KEY).unwrap();
        assert_eq!(claims.user_id, weird);
    }
}
