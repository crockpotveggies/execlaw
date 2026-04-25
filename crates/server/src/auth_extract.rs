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
    pub display_name: String,
    pub email: Option<String>,
    pub role: UserRole,
    pub last_login_at: Option<i64>,
}

impl From<UserRow> for AuthedUser {
    fn from(u: UserRow) -> Self {
        Self {
            user_id: u.user_id,
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
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .ok_or(AuthRejection("missing Authorization header"))?;
        let header_str = header
            .to_str()
            .map_err(|_| AuthRejection("non-ascii Authorization header"))?;
        let token = header_str
            .strip_prefix("Bearer ")
            .ok_or(AuthRejection("Authorization must be 'Bearer <token>'"))?;

        let claims = match state.signer.verify_access_token(token) {
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
