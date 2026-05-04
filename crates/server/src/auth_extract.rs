//! `AuthedUser` extractor — pulls the current logged-in user from
//! the `Authorization: Bearer <jwt>` header on every protected
//! admin route.
//!
//! Verifies the JWT against the server's signing key, looks up the
//! `users` row by `sub`, and short-circuits the request with a 401
//! on any failure (missing header, bad signature, expired token,
//! deleted user). Routes that take `AuthedUser` as an extractor are
//! guaranteed a valid user by the time the handler runs.

use crate::auth::AuthError;
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::IntoResponse;
use axum::response::Response;
use execlaw_core::users::{UserRole, UserRow, UserStore};

/// The current logged-in user. Available as a route extractor.
#[derive(Debug, Clone)]
pub struct AuthedUser {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    pub role: UserRole,
    pub last_login_at: Option<i64>,
}

impl From<UserRow> for AuthedUser {
    fn from(u: UserRow) -> Self {
        Self {
            user_id: u.user_id,
            username: u.username,
            display_name: u.display_name,
            email: u.email,
            role: u.role,
            last_login_at: u.last_login_at,
        }
    }
}

#[derive(Debug)]
pub struct AuthRejection(pub &'static str);

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "unauthorized",
                    "message": self.0,
                }
            })),
        )
            .into_response()
    }
}

impl FromRequestParts<AppState> for AuthedUser {
    type Rejection = AuthRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Primary path: `Authorization: Bearer <jwt>` header. Used
        // by every fetch-driven API call from the SPA + the agent.
        let token_owned: String = match parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
        {
            Some(t) => t.to_owned(),
            None => {
                // Fallback: `?access_token=<jwt>` query parameter.
                // The browser can't carry an Authorization header
                // on link navigations (e.g. `<a download href>` for
                // attachment downloads, video / audio src tags),
                // so the SPA appends the operator's current JWT to
                // the URL as a query param. The token is validated
                // exactly the same way as a header-borne one — same
                // signer.verify_access_token call below — so query-
                // param auth is no weaker than header auth except
                // that the URL ends up in browser history. Mitigate
                // by making the SPA never log download URLs and
                // letting the access token's existing TTL (~minutes)
                // bound the leak window.
                //
                // Standard pattern — used by AWS S3 pre-signed URLs,
                // GitHub raw-blob downloads, etc.
                match query_param(parts.uri.query(), "access_token") {
                    Some(t) => t,
                    None => {
                        return Err(AuthRejection(
                            "missing Authorization header (or ?access_token=… query param)",
                        ));
                    }
                }
            }
        };

        let claims = match state.signer.verify_access_token(&token_owned) {
            Ok(c) => c,
            Err(AuthError::Invalid) | Err(AuthError::Jwt(_)) => {
                return Err(AuthRejection("invalid or expired token"));
            }
            Err(_) => return Err(AuthRejection("token verification failed")),
        };

        let users = UserStore::new(&state.db);
        let row = users
            .get_by_id(&claims.sub)
            .map_err(|_| AuthRejection("user lookup failed"))?
            .ok_or(AuthRejection("token references a user that no longer exists"))?;
        Ok(AuthedUser::from(row))
    }
}

/// Hand-rolled, allocation-light single-key query-string scanner.
/// Avoids pulling in `url::form_urlencoded` for one lookup; handles
/// the common cases (`a=b&c=d`, percent-encoded `+` for spaces,
/// `%XX` decoding). For more sophisticated query-string handling
/// the caller should use `axum::extract::Query<T>` instead.
///
/// Only used by the `AuthedUser` extractor's query-param fallback
/// today; broaden by extracting to a shared helper if a second
/// caller appears.
fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    let q = query?;
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next()?;
        if k != key {
            continue;
        }
        let raw = it.next().unwrap_or("");
        return Some(percent_decode(raw));
    }
    None
}

/// Percent-decode the subset of escapes that show up in JWT
/// query-param values: `%XX` hex escapes and `+` for space. Keeps
/// the impl small (no extra deps) since JWTs are mostly ASCII —
/// only the `=` padding occasionally needs escaping.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_param_extracts_value_among_others() {
        assert_eq!(
            query_param(Some("a=1&access_token=hello&b=2"), "access_token"),
            Some("hello".into()),
        );
    }

    #[test]
    fn query_param_returns_none_when_absent() {
        assert_eq!(query_param(Some("a=1&b=2"), "access_token"), None);
        assert_eq!(query_param(None, "access_token"), None);
    }

    #[test]
    fn percent_decode_handles_jwt_padding_and_dots() {
        // JWTs use `.` separators (no escaping needed) and
        // sometimes pad with `=` (which gets `%3D` escaped).
        assert_eq!(percent_decode("eyJhbGc.payload.sig"), "eyJhbGc.payload.sig");
        assert_eq!(percent_decode("a%3Db"), "a=b");
        assert_eq!(percent_decode("hello+world"), "hello world");
    }

    #[test]
    fn percent_decode_passthrough_on_malformed_escape() {
        // Malformed `%X` (only one hex digit) shouldn't panic.
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%ZZ"), "a%ZZ");
    }
}
