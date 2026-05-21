//! Generic dispatcher for plugin-declared `[[webhook_routes]]`.
//!
//! Mounted at `/api/webhooks/{plugin_id}{path}` with no execlaw
//! JWT check — external services (wuzapi, slack-events, github
//! webhooks, …) can't hold execlaw JWTs. Caller authentication
//! is described by each route's `auth` manifest field and enforced
//! by THIS module BEFORE the request is published to the
//! automation bus or dispatched to the plugin's Rhai handler.
//!
//! This is the deliberate counterpart to `plugin_admin_routes.rs`.
//! The two surfaces are kept strictly disjoint:
//!
//!   * Different URL prefix (`/api/admin/plugins/...` vs
//!     `/api/webhooks/...`)
//!   * Different registry map (`admin_routes` vs `webhook_routes`)
//!   * Different manifest key (`[[admin_routes]]` vs
//!     `[[webhook_routes]]`)
//!
//! so an admin endpoint can never accidentally be served without
//! auth, and a webhook can never be hidden behind auth a third
//! party can't satisfy.
//!
//! Per-request flow:
//!   1. Parse `{plugin_id}` and the trailing path.
//!   2. Look up the plugin's `RegisteredWebhookRoute` set. Match
//!      on (method, path).
//!   3. Decode body to JSON.
//!   4. **Verify caller authentication** per the route's `auth`
//!      decl. On failure, return 401 — NO bus publish, NO handler
//!      dispatch.
//!   5. Publish a (redacted) `WebhookReceived` event to the
//!      automation bus.
//!   6. Invoke the plugin's Rhai handler and return its result.
//!
//! ### Auth modes (set in `plugin.toml`)
//!
//! ```toml
//! [[webhook_routes]]
//! method  = "POST"
//! path    = "/event"
//! handler = "on_webhook_event"
//! # Recommended: constant-time-compare `?token=` against a vault row.
//! auth = { kind = "query_token", query = "token", vault_key = "webhook_secret" }
//! # Or, for GitHub-style HMAC-signed bodies:
//! # auth = { kind = "hmac_sha256_header", header = "X-Hub-Signature-256", vault_key = "github_webhook_secret" }
//! # Or, for explicit opt-out (handler validates):
//! # auth = { kind = "none" }
//! ```
//!
//! Omitting `auth` entirely is permitted for backward compatibility
//! with pre-2026-05 plugins but logs a `webhook_route_auth_unset`
//! warning at plugin enable AND on every webhook hit, and falls back
//! to the legacy "handler validates" model. The dispatcher still
//! redacts a small denylist of common secret-bearing query keys
//! (`token`, `signature`, `auth`, …) from the persisted bus payload
//! so accidental in-URL secrets don't end up in the durable log.

use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::any;
use execlaw_plugin_sdk::manifest::WebhookAuthDecl;
use std::collections::BTreeMap;

/// Query-string keys whose values are always redacted from the
/// persisted automation-bus payload, regardless of whether the route
/// declared an `auth` mode. Covers the common name-confusion footguns
/// (`token`, `signature`, `auth`, …) so an in-URL secret never lands
/// in the durable event log. Additive: a route-declared `auth.query`
/// key is also redacted on top of this list.
const ALWAYS_REDACTED_QUERY_KEYS: &[&str] = &[
    "token",
    "access_token",
    "auth",
    "auth_token",
    "secret",
    "signature",
    "sig",
    "api_key",
    "apikey",
    "key",
    "password",
    "webhook_secret",
];

/// Constant-time byte-slice equality. Returns false on length mismatch
/// without an early-exit timing channel.
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

/// Build the structured 401 response. Generic message so a probing
/// caller can't tell whether the route exists, the secret is wrong,
/// or the vault is misconfigured.
fn unauthorized(plugin_id: &str, reason: &'static str) -> ApiError {
    tracing::warn!(
        target: "plugin_webhook_routes",
        plugin_id = %plugin_id,
        reason,
        "webhook auth rejected"
    );
    ApiError {
        status: StatusCode::UNAUTHORIZED,
        code: "plugin_webhook_unauthorized",
        message: "webhook authentication failed".to_owned(),
    }
}

/// Resolve a per-plugin vault secret. Returns `Err(401)` on missing,
/// empty, or non-UTF-8 values — all are treated as "not authenticated"
/// rather than "no auth required".
fn resolve_vault_secret(
    state: &AppState,
    plugin_id: &str,
    vault_key: &str,
) -> Result<String, ApiError> {
    use execlaw_core::vault_row::VaultRowStore;
    let store = VaultRowStore::new(&state.db);
    let raw = store.get(Some(plugin_id), vault_key).map_err(|e| {
        tracing::error!(
            target: "plugin_webhook_routes",
            plugin_id, vault_key, error = %e,
            "vault read failed during webhook auth"
        );
        unauthorized(plugin_id, "vault_read_failed")
    })?;
    let bytes = raw.ok_or_else(|| unauthorized(plugin_id, "vault_secret_missing"))?;
    if bytes.is_empty() {
        return Err(unauthorized(plugin_id, "vault_secret_empty"));
    }
    String::from_utf8(bytes).map_err(|_| unauthorized(plugin_id, "vault_secret_not_utf8"))
}

/// Host-enforced authentication for one inbound webhook hit. Returns
/// `Ok(())` when the caller is authenticated under the route's
/// declared `auth` mode, or a 401 `ApiError` otherwise.
///
/// `auth = None` (manifest omitted the field) is the legacy "handler
/// validates" path — returns Ok and lets the dispatcher continue to
/// bus publish + handler. The dispatcher still redacts secrets from
/// the bus payload in that case.
fn verify_webhook_auth(
    state: &AppState,
    plugin_id: &str,
    auth: Option<&WebhookAuthDecl>,
    query: &BTreeMap<String, String>,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), ApiError> {
    let Some(auth) = auth else {
        // Legacy path. Loud per-request warning so an operator grep'ing
        // for `webhook_route_auth_unset` finds every hit, not just
        // every plugin enable.
        tracing::warn!(
            target: "plugin_webhook_routes",
            plugin_id, "webhook_route_auth_unset: relying on handler-side validation"
        );
        return Ok(());
    };
    match auth {
        WebhookAuthDecl::None => Ok(()),
        WebhookAuthDecl::QueryToken {
            query: q_key,
            vault_key,
        } => {
            let expected = resolve_vault_secret(state, plugin_id, vault_key)?;
            let supplied = query
                .get(q_key.as_str())
                .ok_or_else(|| unauthorized(plugin_id, "query_token_missing"))?;
            if !ct_eq(expected.as_bytes(), supplied.as_bytes()) {
                return Err(unauthorized(plugin_id, "query_token_mismatch"));
            }
            Ok(())
        }
        WebhookAuthDecl::HmacSha256Header { header, vault_key } => {
            use hmac::{Hmac, Mac};
            use sha2::Sha256;
            let supplied = headers
                .get(header.as_str())
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| unauthorized(plugin_id, "hmac_header_missing"))?;
            // GitHub & friends prefix with `sha256=`; strip if present
            // so the operator can register either form.
            let supplied_hex = supplied.strip_prefix("sha256=").unwrap_or(supplied);
            let supplied_bytes =
                hex::decode(supplied_hex).map_err(|_| unauthorized(plugin_id, "hmac_not_hex"))?;
            let secret = resolve_vault_secret(state, plugin_id, vault_key)?;
            let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes())
                .map_err(|_| unauthorized(plugin_id, "hmac_key_invalid"))?;
            mac.update(body);
            mac.verify_slice(&supplied_bytes)
                .map_err(|_| unauthorized(plugin_id, "hmac_mismatch"))?;
            Ok(())
        }
    }
}

/// Build the redacted query map that gets persisted to the automation
/// bus. The original `query` BTreeMap is left untouched (handlers may
/// still legitimately need the token, e.g. an HMAC route that wants
/// to echo a challenge). Only the bus-event copy is scrubbed.
fn redact_query_for_bus(
    query: &BTreeMap<String, String>,
    auth: Option<&WebhookAuthDecl>,
) -> serde_json::Value {
    let extra_redact: Option<&str> = match auth {
        Some(WebhookAuthDecl::QueryToken { query: k, .. }) => Some(k.as_str()),
        _ => None,
    };
    let map: serde_json::Map<String, serde_json::Value> = query
        .iter()
        .map(|(k, v)| {
            let lower = k.to_ascii_lowercase();
            let redact = ALWAYS_REDACTED_QUERY_KEYS.contains(&lower.as_str())
                || extra_redact
                    .map(|e| e.eq_ignore_ascii_case(k))
                    .unwrap_or(false);
            let val = if redact {
                serde_json::Value::String("<redacted>".to_owned())
            } else {
                serde_json::Value::String(v.clone())
            };
            (k.clone(), val)
        })
        .collect();
    serde_json::Value::Object(map)
}

/// Mount the catch-all under `/api/webhooks/:plugin_id/...`.
/// Match-anything `*tail` lets a plugin declare nested paths.
pub(crate) fn webhook_routes_router() -> Router<AppState> {
    Router::new().route("/api/webhooks/{plugin_id}/{*tail}", any(dispatch_handler))
}

async fn dispatch_handler(
    State(state): State<AppState>,
    Path((plugin_id, tail)): Path<(String, String)>,
    method: Method,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let path_with_slash = format!("/{tail}");

    // Look up the matching webhook_route declaration. NOTE: we
    // intentionally do NOT fall back to admin_routes — admin
    // routes are auth-gated by /api/admin/plugins/... and a
    // webhook hit must match a public webhook decl, never an
    // admin one.
    let routes = state.plugin_host.registry().webhook_routes_for(&plugin_id);
    let upper = method.as_str().to_uppercase();
    let decl = routes
        .into_iter()
        .find(|r| r.method == upper && r.path == path_with_slash)
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "plugin_webhook_route_not_found",
            message: format!(
                "no [[webhook_routes]] entry on plugin '{plugin_id}' \
                 matching {upper} {path_with_slash}"
            ),
        })?;

    // Host-enforced auth runs immediately after route lookup, BEFORE
    // any other side effect: no script-plugin lookup, no body decode,
    // no automation-bus publish, no handler invocation. The audit's
    // critical finding — "unauthenticated webhooks hit the automation
    // bus before plugin auth" — is fixed by this position.
    verify_webhook_auth(
        &state,
        &plugin_id,
        decl.auth.as_ref(),
        &query,
        &headers,
        body.as_ref(),
    )?;

    // Look up the live script plugin.
    let plugin = state
        .plugin_host
        .script_plugin(&plugin_id)
        .await
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "plugin_not_loaded",
            message: format!(
                "plugin '{plugin_id}' is registered but not loaded as a script plugin"
            ),
        })?;

    // Decode body to a JSON value the plugin can pattern-match on.
    // Three encodings, by order of probable Content-Type:
    //
    //   * application/json — most webhooks (Slack, GitHub, Stripe).
    //   * application/x-www-form-urlencoded — wuzapi (whatsmeow
    //     wrapper) posts `instanceName=...&jsonData=<urlencoded
    //     JSON>&userID=...`. We decode it into the same shape a
    //     JSON post would produce, so plugin code stays uniform.
    //   * Anything else — fall through to a UTF-8 String so the
    //     plugin can decide.
    //
    // If Content-Type is missing/garbage we still try JSON first
    // because Slack-style senders sometimes omit it.
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    let body_value: serde_json::Value = if body.is_empty() {
        serde_json::Value::Null
    } else if content_type == "application/x-www-form-urlencoded"
        || (content_type.is_empty() && body.first() != Some(&b'{') && body.first() != Some(&b'['))
    {
        // serde_urlencoded::from_bytes -> Vec<(String, String)>.
        // Build a JSON object so plugins see `body.fieldName`
        // exactly as they would for a JSON post. wuzapi puts its
        // payload inside `jsonData` as a (URL-decoded) JSON
        // string — the plugin's handler is already expected to
        // re-parse that with parse_json.
        match serde_urlencoded::from_bytes::<Vec<(String, String)>>(&body) {
            Ok(pairs) => {
                let map: serde_json::Map<String, serde_json::Value> = pairs
                    .into_iter()
                    .map(|(k, v)| (k, serde_json::Value::String(v)))
                    .collect();
                serde_json::Value::Object(map)
            }
            Err(e) => {
                tracing::warn!(
                    plugin_id = %plugin_id,
                    body_len = body.len(),
                    body_preview = %String::from_utf8_lossy(&body[..body.len().min(400)]),
                    parse_err = %e,
                    "webhook body not form-urlencoded; falling back to String"
                );
                serde_json::Value::String(String::from_utf8_lossy(&body).to_string())
            }
        }
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    plugin_id = %plugin_id,
                    content_type = %content_type,
                    body_len = body.len(),
                    body_preview = %String::from_utf8_lossy(&body[..body.len().min(400)]),
                    parse_err = %e,
                    "webhook body not JSON; falling back to String"
                );
                serde_json::Value::String(String::from_utf8_lossy(&body).to_string())
            }
        }
    };

    let query_value_full = serde_json::Value::Object(
        query
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect(),
    );
    let query_value_redacted = redact_query_for_bus(&query, decl.auth.as_ref());
    let args = serde_json::json!({
        "method": upper,
        "path": path_with_slash,
        "query": query_value_full,
        "body": body_value,
    });

    // M1 of Automations — emit a `WebhookReceived` event on the
    // durable automation bus alongside (NOT instead of) the existing
    // plugin Rhai handler dispatch. The bus emission is best-effort:
    // a failure to publish must NOT block webhook handling, since the
    // upstream caller has no idea this bus even exists. Dedup key is
    // a deterministic hash over (plugin_id, method, path, body) so
    // upstream retries collapse into one event.
    //
    // We do the publish AFTER auth verification (above) so an
    // unauthenticated probe can't pollute the durable bus, but BEFORE
    // invoking the handler so a slow / hung handler doesn't delay
    // automation observability. Sensitive query keys are redacted
    // from the persisted payload (`query_value_redacted`) — `args`
    // passed to the handler keeps the original values.
    {
        use sha2::{Digest, Sha256};
        let dedup_id = {
            let mut h = Sha256::new();
            h.update(plugin_id.as_bytes());
            h.update(b":");
            h.update(upper.as_bytes());
            h.update(b":");
            h.update(path_with_slash.as_bytes());
            h.update(b":");
            h.update(&body);
            format!("webhook:{plugin_id}:{:x}", h.finalize())
        };
        let evt = execlaw_core::automation_bus::Event {
            id: dedup_id,
            kind: execlaw_core::automation_bus::BusEventKind::WebhookReceived,
            source: format!("webhook:{plugin_id}"),
            received_at: chrono::Utc::now().timestamp_millis(),
            payload: serde_json::json!({
                "plugin_id": plugin_id,
                "method": upper,
                "path": path_with_slash,
                "query": query_value_redacted,
                "body": body_value.clone(),
            }),
            // TODO(M6 Phase 6): populate envelope.origin with a
            // PluginChannel ref so flows can `SendReply` back through
            // the webhook caller. For now webhooks remain
            // fire-and-forget (Phase 5+ migration).
            envelope: None,
        };
        if let Err(e) = state.automation_bus.publish(evt).await {
            tracing::warn!(
                plugin_id = %plugin_id,
                error = %e,
                "automation bus: webhook publish failed (handler dispatch continues)",
            );
        }
    }

    use execlaw_script::primitives_glue::json_to_rhai;
    let dyn_args = vec![json_to_rhai(&args)];
    let result = plugin
        .invoke_async_owned(decl.handler.clone(), dyn_args)
        .await
        .map_err(|e| ApiError {
            // Webhook callers don't read execlaw's error semantics —
            // they want a 200/non-200 distinction. We use 500 on
            // handler error so wuzapi-style retry logic kicks in if
            // the third party retries.
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "plugin_webhook_handler_error",
            message: format!("[{plugin_id}] handler {}: {e}", decl.handler),
        })?;
    Ok((StatusCode::OK, Json(result)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_matches_only_on_full_equality() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"abc", b""));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn redact_strips_common_secret_query_keys() {
        let mut q = BTreeMap::new();
        q.insert("token".into(), "shhh".into());
        q.insert("Signature".into(), "ABCD".into());
        q.insert("user".into(), "alice".into());
        let red = redact_query_for_bus(&q, None);
        let obj = red.as_object().unwrap();
        assert_eq!(obj["token"], "<redacted>");
        assert_eq!(obj["Signature"], "<redacted>");
        assert_eq!(obj["user"], "alice");
    }

    #[test]
    fn redact_also_strips_route_declared_auth_query_key() {
        let mut q = BTreeMap::new();
        q.insert("verify".into(), "shhh".into());
        q.insert("user".into(), "alice".into());
        let auth = WebhookAuthDecl::QueryToken {
            query: "verify".into(),
            vault_key: "k".into(),
        };
        let red = redact_query_for_bus(&q, Some(&auth));
        let obj = red.as_object().unwrap();
        assert_eq!(obj["verify"], "<redacted>");
        assert_eq!(obj["user"], "alice");
    }
}
