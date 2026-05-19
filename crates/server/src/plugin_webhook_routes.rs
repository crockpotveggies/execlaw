//! Generic dispatcher for plugin-declared `[[webhook_routes]]`.
//!
//! Mounted UNAUTHENTICATED at `/api/webhooks/{plugin_id}{path}` —
//! external services (wuzapi, slack-events, github webhooks, …)
//! can't hold execlaw JWTs, so the HTTP layer skips the
//! `AuthedUser` extractor entirely. The plugin's Rhai handler is
//! responsible for verifying the request's authenticity, typically
//! by matching a `?token=` query param against a vault-stored
//! shared secret.
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
//! Per-request flow mirrors the admin dispatcher:
//!   1. Parse `{plugin_id}` and the trailing path.
//!   2. Look up the plugin's `RegisteredWebhookRoute` set. Match
//!      on (method, path).
//!   3. Build a Rhai args map: `{method, path, query, body}` and
//!      hand it to the plugin's handler.
//!   4. Return whatever the handler returned as JSON.
//!
//! ### Operator-facing security note
//!
//! Because this surface is reachable from anyone who can reach
//! the host's bind address, plugin handlers MUST validate the
//! caller before doing anything stateful. The convention in-tree
//! is:
//!
//! ```text
//! POST /api/webhooks/whatsapp/event?token=<random>
//! ```
//!
//! where `<random>` is a per-install secret the plugin generates
//! at first enable, persists in its vault, and inserts into the
//! third-party service's webhook configuration. The handler does
//! `vault_get("webhook_secret")` and compares against the query
//! param. Don't log the token. Don't accept the token in headers
//! the third party can't control.

use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::any;
use std::collections::BTreeMap;

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
    let query_value = serde_json::Value::Object(
        query
            .into_iter()
            .map(|(k, v)| (k, serde_json::Value::String(v)))
            .collect(),
    );
    let args = serde_json::json!({
        "method": upper,
        "path": path_with_slash,
        "query": query_value,
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
    // We do the publish BEFORE invoking the handler so a slow / hung
    // handler doesn't delay automation observability, but AFTER body
    // parsing so the payload field carries the same shape the plugin
    // sees.
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
                "query": query_value.clone(),
                "body": body_value.clone(),
            }),
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
