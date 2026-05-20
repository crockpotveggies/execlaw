//! `POST /api/downloads/sign` — mint a short-lived signed URL for
//! a browser-direct GET.
//!
//! See [`download_urls`](crate::download_urls) for the URL shape +
//! verification flow. This endpoint is the only mechanism the SPA
//! uses to obtain those URLs: `<a download>`, `<img src>`,
//! `<video src>` etc. all read from `useEffect`-resolved values that
//! were obtained from here.
//!
//! Auth: `AuthedUser` (header-only — query-token auth was removed
//! when this endpoint landed). The returned URL is bound to the
//! current operator; another user can't use it.
//!
//! Path allowlist: the request body's `path` must match one of the
//! safe prefixes below. Without an allowlist the sign endpoint would
//! be a generic "sign anything" oracle the SPA could be tricked into
//! signing (a compromised plugin panel calling
//! `POST /api/downloads/sign { path: "/api/admin/factory-reset" }`
//! would otherwise mint a working delete-everything URL). The
//! allowlist contains exactly the paths a browser-direct GET is
//! legitimate for.

use crate::auth_extract::AuthedUser;
use crate::download_urls::{DEFAULT_TTL_SECS, build_signed_url};
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::post;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct SignRequest {
    pub path: String,
    /// Optional override on the default TTL. Clamped server-side
    /// to `[1, MAX_TTL_SECS]`; values outside the range get clamped
    /// rather than rejected so the SPA doesn't need to know the cap.
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct SignResponse {
    /// Fully assembled URL the SPA pastes into `<a download>` /
    /// `<img src>`. Same-origin so no CORS surface.
    pub url: String,
    /// Absolute expiry, unix seconds — surfaceable as a "valid until"
    /// hint or for SPA-side re-sign scheduling.
    pub expires_at: i64,
}

/// Path prefixes the sign endpoint is willing to sign. Update this
/// list when a NEW browser-direct GET route lands. Anything not on
/// the list returns 403, even to an authenticated operator.
const ALLOWED_PREFIXES: &[&str] = &["/api/attachments/"];

fn path_is_allowed(p: &str) -> bool {
    // Defensive: reject anything with `?`, `#`, or path-traversal
    // tokens BEFORE the prefix check. A caller passing
    // `/api/attachments/../admin/factory-reset` would otherwise
    // pass the prefix match and yield a sig over an unsafe path.
    if p.contains('?') || p.contains('#') || p.contains("..") {
        return false;
    }
    if !p.starts_with('/') {
        return false;
    }
    ALLOWED_PREFIXES.iter().any(|prefix| {
        // Require AT LEAST one char after the prefix so empty-id
        // paths (`/api/attachments/`) don't slip through.
        p.starts_with(prefix) && p.len() > prefix.len()
    })
}

pub async fn sign_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<SignRequest>,
) -> Result<Response, ApiError> {
    if !path_is_allowed(&req.path) {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            code: "download_path_not_allowed",
            message: format!(
                "path '{}' is not on the signed-download allowlist; \
                 only attachment GETs may be signed",
                req.path
            ),
        });
    }
    let ttl = req.ttl_secs.unwrap_or(DEFAULT_TTL_SECS);
    let (url, expires_at) = build_signed_url(
        &req.path,
        &user.user_id,
        ttl,
        state.signer.download_hmac_key(),
    );
    Ok((StatusCode::OK, Json(SignResponse { url, expires_at })).into_response())
}

pub fn downloads_router() -> Router<AppState> {
    Router::new().route("/api/downloads/sign", post(sign_handler))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_accepts_real_attachment_paths() {
        assert!(path_is_allowed("/api/attachments/abc-123"));
        assert!(path_is_allowed("/api/attachments/u_with-mixed-chars"));
    }

    #[test]
    fn allowlist_rejects_paths_outside_prefix() {
        assert!(!path_is_allowed("/api/admin/users"));
        assert!(!path_is_allowed("/api/setup"));
        assert!(!path_is_allowed("/"));
        assert!(!path_is_allowed(""));
    }

    #[test]
    fn allowlist_rejects_empty_attachment_id() {
        // No id segment after the prefix — would yield a sig over
        // a path that doesn't even resolve to a row.
        assert!(!path_is_allowed("/api/attachments/"));
    }

    #[test]
    fn allowlist_rejects_path_traversal_attempts() {
        assert!(!path_is_allowed("/api/attachments/../admin/factory-reset"));
        assert!(!path_is_allowed("/api/attachments/foo/../../admin"));
    }

    #[test]
    fn allowlist_rejects_query_or_fragment_in_path() {
        // The sign endpoint signs the raw path; a `?` would
        // confuse the URL-assembly downstream and could be used to
        // shadow the exp/user/sig params.
        assert!(!path_is_allowed("/api/attachments/x?foo=bar"));
        assert!(!path_is_allowed("/api/attachments/x#frag"));
    }

    #[test]
    fn allowlist_requires_absolute_path() {
        assert!(!path_is_allowed("api/attachments/x"));
        assert!(!path_is_allowed("http://example.com/api/attachments/x"));
    }
}
