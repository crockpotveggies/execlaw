//! Signal plugin admin surface (Phase 8 — operator pairing UI).
//!
//! Routes:
//!   * `GET    /api/admin/signal/status` — sidecar health +
//!     registered-account list, fetched live from the supervised
//!     signal-cli sidecar's `/v1/accounts`.
//!   * `GET    /api/admin/signal/qrcodelink?device_name=execlaw` —
//!     proxy to the sidecar's `GET /v1/qrcodelink`, returns the PNG
//!     bytes verbatim. The operator's phone scans this with
//!     Signal → Settings → Linked devices → Link new device.
//!   * `DELETE /api/admin/signal/accounts/{number}` — proxy to
//!     `DELETE /v1/unregister/{number}`, removing execlaw's link to
//!     the account. Controller-only; the operator confirms before
//!     hitting it.
//!
//! All endpoints are Controller-only and dial the supervised
//! sidecar via `SidecarSupervisor::host_port_for(SIGNAL_SIDECAR_NAME)`.
//! When the sidecar isn't running yet (Stopped / CrashLooping / not
//! published), the routes return 503 with a clear message so the
//! SPA can show "Sidecar not running — check Settings → Sidecars."

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::signal_transport::SIGNAL_SIDECAR_NAME;
use crate::state::AppState;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get};
use execlaw_core::users::UserRole;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct SignalStatusResponse {
    /// Live sidecar status reflected from the supervisor. Mirrors
    /// `crate::sidecar_supervisor::SidecarRuntimeStatus.status` but
    /// flattened to a string the SPA renders directly.
    pub sidecar_status: String,
    /// Loopback URL the supervisor published. `None` until the
    /// first successful spawn (the SPA shows a "starting" hint).
    pub sidecar_rpc_url: Option<String>,
    /// Phone numbers signal-cli has on file (`/v1/accounts`).
    /// Empty when no account is registered yet — the SPA's pairing
    /// flow surfaces a QR-code link affordance.
    pub registered_accounts: Vec<String>,
    /// Set when the sidecar isn't reachable so the SPA can surface
    /// the underlying error verbatim instead of trying to render a
    /// blank "no accounts" view.
    pub fetch_error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QrCodeLinkQuery {
    /// Operator-visible device name shown in the phone's "Linked
    /// devices" list once pairing completes. Defaults to "execlaw"
    /// — the operator can override per-deployment if they run
    /// multiple installs against the same Signal account.
    #[serde(default = "default_device_name")]
    device_name: String,
}

fn default_device_name() -> String {
    "execlaw".to_owned()
}

/// Build the supervised sidecar's loopback base URL, or surface a
/// 503 when it isn't running yet. The SPA distinguishes this from
/// other 5xx so the "sidecar starting" hint can render specifically.
async fn resolve_sidecar_url(state: &AppState) -> Result<String, ApiError> {
    let supervisor = state.sidecar_supervisor.as_ref().ok_or_else(|| ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "supervisor_unwired",
        message: "sidecar supervisor is not running (Docker unreachable?)".into(),
    })?;
    match supervisor.host_port_for(SIGNAL_SIDECAR_NAME).await {
        Some(p) => Ok(format!("http://127.0.0.1:{p}")),
        None => Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "sidecar_not_running",
            message: format!(
                "signal-cli sidecar '{SIGNAL_SIDECAR_NAME}' has no published host port yet — \
                 wait for the supervisor to bring it up, or check Settings → Sidecars"
            ),
        }),
    }
}

/// Reqwest client for outbound proxies. Localhost loopback so we
/// can be aggressive on connect timeouts without surprising the
/// operator. Built fresh per request (cheap; no connection-pool
/// re-use win on a 1-shot proxy call).
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[utoipa::path(
    get,
    path = "/api/admin/signal/status",
    responses((status = 200, description = "Signal sidecar + account status", body = SignalStatusResponse)),
    security(("bearer_jwt" = [])),
    tag = "signal-admin"
)]
pub async fn status_handler(
    State(state): State<AppState>,
    user: AuthedUser,
) -> Result<Json<SignalStatusResponse>, ApiError> {
    require_controller(&user)?;
    // Pull the supervisor snapshot first — that gives us the
    // user-facing status string regardless of whether the RPC
    // probe below succeeds.
    let snapshot = match state.sidecar_supervisor.as_ref() {
        Some(s) => s
            .snapshot_status()
            .await
            .into_iter()
            .find(|r| r.name == SIGNAL_SIDECAR_NAME),
        None => None,
    };
    let (sidecar_status, sidecar_rpc_url) = match snapshot {
        Some(s) => (format!("{:?}", s.status).to_lowercase(), s.rpc_url),
        None => ("unwired".into(), None),
    };
    // Best-effort accounts fetch. A 5xx / connect-refused here
    // doesn't fail the whole status response — the SPA still wants
    // to render the sidecar status string even when /v1/accounts
    // is unreachable.
    let mut accounts = Vec::new();
    let mut fetch_error: Option<String> = None;
    if let Some(url) = &sidecar_rpc_url {
        match fetch_accounts(url).await {
            Ok(v) => accounts = v,
            Err(e) => fetch_error = Some(e),
        }
    }
    Ok(Json(SignalStatusResponse {
        sidecar_status,
        sidecar_rpc_url,
        registered_accounts: accounts,
        fetch_error,
    }))
}

/// GET `/v1/accounts` against the supervised sidecar. Returns the
/// list of E.164 phone numbers signal-cli has on file. Empty list
/// is the "no account paired yet" state the SPA renders the QR-code
/// affordance for.
async fn fetch_accounts(base_url: &str) -> Result<Vec<String>, String> {
    let url = format!("{base_url}/v1/accounts");
    let resp = client()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("dial signal-cli: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "signal-cli /v1/accounts returned HTTP {status}: {}",
            truncate(&body, 240)
        ));
    }
    // signal-cli-rest-api returns a bare JSON array of strings.
    let parsed: Vec<String> = resp
        .json()
        .await
        .map_err(|e| format!("parse signal-cli /v1/accounts: {e}"))?;
    Ok(parsed)
}

#[utoipa::path(
    get,
    path = "/api/admin/signal/qrcodelink",
    params(("device_name" = String, Query, description = "Operator-visible device label")),
    responses((status = 200, description = "QR code PNG", content_type = "image/png")),
    security(("bearer_jwt" = [])),
    tag = "signal-admin"
)]
pub async fn qrcodelink_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Query(q): Query<QrCodeLinkQuery>,
) -> Result<Response, ApiError> {
    require_controller(&user)?;
    let base_url = resolve_sidecar_url(&state).await?;
    // signal-cli-rest-api's /v1/qrcodelink accepts a `device_name`
    // query param; sanitise to alphanumeric-ish so a hostile
    // upstream value (theoretically operator-controlled) can't
    // smuggle URL-injection bytes into the proxy hop.
    let safe_name = sanitise_device_name(&q.device_name);
    // Sanitiser already restricts to URL-safe chars (alphanumeric +
    // ` -_`); only spaces need explicit `%20` encoding for the
    // outbound URL. Keeps us off a third-party percent-encode dep.
    let encoded = safe_name.replace(' ', "%20");
    let url = format!("{base_url}/v1/qrcodelink?device_name={encoded}");
    let resp = client().get(&url).send().await.map_err(|e| ApiError {
        status: StatusCode::BAD_GATEWAY,
        code: "sidecar_unreachable",
        message: format!("dial signal-cli: {e}"),
    })?;
    let upstream_status = resp.status();
    if !upstream_status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ApiError {
            status: StatusCode::BAD_GATEWAY,
            code: "sidecar_qr_failed",
            message: format!(
                "signal-cli /v1/qrcodelink returned HTTP {upstream_status}: {}",
                truncate(&body, 240)
            ),
        });
    }
    // Stream-through the response. signal-cli returns the PNG
    // bytes directly; we forward Content-Type + body.
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/png")
        .to_owned();
    let bytes = resp.bytes().await.map_err(|e| ApiError {
        status: StatusCode::BAD_GATEWAY,
        code: "sidecar_qr_body",
        message: format!("read /v1/qrcodelink body: {e}"),
    })?;
    let mut headers = HeaderMap::new();
    if let Ok(v) = header::HeaderValue::from_str(&content_type) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    // Cache-Control: no-store. The QR code embeds a short-lived
    // pairing nonce; serving a cached image would let a stale
    // browser tab pair against an already-expired challenge.
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    Ok((StatusCode::OK, headers, Body::from(bytes)).into_response())
}

#[utoipa::path(
    delete,
    path = "/api/admin/signal/accounts/{number}",
    responses((status = 204, description = "Account dropped from the sidecar")),
    security(("bearer_jwt" = [])),
    tag = "signal-admin"
)]
pub async fn unregister_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(number): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    require_controller(&user)?;
    let base_url = resolve_sidecar_url(&state).await?;
    if !is_valid_e164(&number) {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_number",
            message: "number must be E.164 (`+` followed by 1–15 digits)".into(),
        });
    }
    // E.164 validation already constrained to `+` + digits; the
    // `+` is URL-path-safe in practice but spelling it as `%2B`
    // dodges any router that might decode `+` as a space.
    let encoded = number.replace('+', "%2B");
    let url = format!("{base_url}/v1/unregister/{encoded}");
    let resp = client().delete(&url).send().await.map_err(|e| ApiError {
        status: StatusCode::BAD_GATEWAY,
        code: "sidecar_unreachable",
        message: format!("dial signal-cli: {e}"),
    })?;
    let upstream = resp.status();
    if !upstream.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ApiError {
            status: StatusCode::BAD_GATEWAY,
            code: "sidecar_unregister_failed",
            message: format!(
                "signal-cli /v1/unregister returned HTTP {upstream}: {}",
                truncate(&body, 240)
            ),
        });
    }
    Ok(StatusCode::NO_CONTENT)
}

fn require_controller(user: &AuthedUser) -> Result<(), ApiError> {
    // The pairing flow can drop an account, fetch a one-time QR
    // pairing nonce, and read the registered phone number — all
    // controller-only operations.
    if user.role == UserRole::Controller {
        Ok(())
    } else {
        Err(ApiError {
            status: StatusCode::FORBIDDEN,
            code: "controller_only",
            message: "signal admin operations are Controller-only".into(),
        })
    }
}

fn is_valid_e164(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'+') {
        return false;
    }
    let digits = &bytes[1..];
    !digits.is_empty() && digits.len() <= 15 && digits.iter().all(|b| b.is_ascii_digit())
}

/// Strip device-name to ASCII alphanumeric + `-_ ` so a malicious
/// query param can't inject URL bytes into the proxy hop. The
/// urlencoding crate would catch most of this on its own, but
/// belt-and-suspenders is cheap and lets us cap length on the
/// way through.
fn sanitise_device_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(*c, ' ' | '-' | '_'))
        .take(64)
        .collect();
    if cleaned.trim().is_empty() {
        "execlaw".to_owned()
    } else {
        cleaned
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

pub fn signal_admin_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/signal/status", get(status_handler))
        .route("/api/admin/signal/qrcodelink", get(qrcodelink_handler))
        .route(
            "/api/admin/signal/accounts/{number}",
            delete(unregister_handler),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitise_device_name_strips_special_chars() {
        assert_eq!(sanitise_device_name("execlaw"), "execlaw");
        assert_eq!(sanitise_device_name("My Laptop"), "My Laptop");
        assert_eq!(sanitise_device_name("evil; rm -rf"), "evil rm -rf");
        assert_eq!(sanitise_device_name("a/b/c"), "abc");
        assert_eq!(sanitise_device_name(""), "execlaw");
        assert_eq!(sanitise_device_name("   "), "execlaw");
    }

    #[test]
    fn sanitise_device_name_caps_length() {
        let big = "a".repeat(200);
        assert_eq!(sanitise_device_name(&big).len(), 64);
    }

    #[test]
    fn is_valid_e164_accepts_canonical_numbers_and_rejects_garbage() {
        assert!(is_valid_e164("+15551234567"));
        assert!(is_valid_e164("+1"));
        assert!(!is_valid_e164(""));
        assert!(!is_valid_e164("15551234567"));
        assert!(!is_valid_e164("+1-555-1234"));
        assert!(!is_valid_e164("+abc"));
        assert!(!is_valid_e164("../etc/passwd"));
    }

    #[test]
    fn truncate_caps_long_strings() {
        let big = "x".repeat(500);
        let out = truncate(&big, 240);
        assert_eq!(out.chars().count(), 241);
        assert!(out.ends_with('…'));
    }
}
