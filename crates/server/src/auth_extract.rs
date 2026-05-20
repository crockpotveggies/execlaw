//! `AuthedUser` extractor — pulls the current logged-in user from
//! the `Authorization: Bearer <jwt>` header on every protected
//! admin route.
//!
//! Verifies the JWT against the server's signing key, looks up the
//! `users` row by `sub`, and short-circuits the request with a 401
//! on any failure (missing header, bad signature, expired token,
//! deleted user). Routes that take `AuthedUser` as an extractor are
//! guaranteed a valid user by the time the handler runs.
//!
//! **2026-05-19 security fix**: the previous `?access_token=<jwt>`
//! query-string fallback was removed (audit finding: full-access JWTs
//! travelled through browser history, proxy logs, referrers, and
//! copied-link surfaces). Routes the browser hits directly (`<a
//! download>`, `<img src>`, video src) now use `MediaAuthedUser`
//! from [`download_urls`](crate::download_urls), which accepts the
//! Authorization header OR a short-lived signed URL — never a raw
//! JWT in the URL.

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
        // Only path: `Authorization: Bearer <jwt>` header. The
        // pre-2026-05 `?access_token=…` query fallback was removed —
        // full-access JWTs in query strings leaked through browser
        // history, proxy logs, referrers, and copied-link surfaces.
        // Routes the browser hits directly should switch to
        // `MediaAuthedUser` (see crate::download_urls), which accepts
        // a short-lived signed URL instead of a raw JWT.
        let token_owned: String = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or(AuthRejection("missing Authorization header"))?
            .to_owned();

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
            .ok_or(AuthRejection(
                "token references a user that no longer exists",
            ))?;
        Ok(AuthedUser::from(row))
    }
}
