//! Generic dispatcher for plugin-declared `[[admin_routes]]`.
//!
//! A plugin's manifest may declare admin endpoints under
//! `/api/admin/plugins/{plugin_id}{path}` that map to a Rhai
//! handler in the plugin's script. This module mounts a single
//! catch-all axum route + dispatches each request to the matching
//! handler via `ScriptPlugin::invoke_async_owned`.
//!
//! Per-request flow:
//!
//!   1. Parse `{plugin_id}` and the trailing path from the URL.
//!   2. Look up the plugin's `RegisteredAdminRoute` set in
//!      [`HookRegistry::admin_routes_for`]. Match on (method, path).
//!   3. Look up the live `ScriptPlugin` via the host.
//!   4. Build a Rhai args map: `{method, path, query, body, headers}`.
//!   5. Invoke the handler. Return its JSON output as the HTTP body.
//!
//! All plugin admin endpoints require Controller trust — the
//! existing `AuthedUser` extractor handles authentication; we
//! gate Controller-only inside this dispatcher.
//!
//! ### What we don't (yet) do
//!
//! * No streaming responses — handlers return complete JSON. Fine
//!   for the typical use case (pairing status, QR PNG response in
//!   base64, etc.).
//! * No header passthrough into the handler beyond a small allow-
//!   list. A plugin shouldn't need anything more; future expansion
//!   can pass select headers as a `headers` map field.
//! * No per-route auth knobs in the manifest. All routes are
//!   Controller-only. Open this up later if a use case demands it.

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::any;
use std::collections::BTreeMap;

fn require_controller(state: &AppState, user: &AuthedUser) -> Result<(), ApiError> {
    use execlaw_core::users::{UserRole, UserStore};
    let row = UserStore::new(&state.db)
        .get_by_id(&user.user_id)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "db_error",
            message: e.to_string(),
        })?;
    match row.map(|u| u.role) {
        Some(UserRole::Controller) => Ok(()),
        _ => Err(ApiError {
            status: StatusCode::FORBIDDEN,
            code: "controller_required",
            message: "Controller-only endpoint".into(),
        }),
    }
}

/// Mount the catch-all under `/api/admin/plugins/:plugin_id/...`.
/// Match-anything `*tail` lets a plugin declare nested paths
/// (`/pair/start`, `/pair/finalize`).
pub(crate) fn admin_routes_router() -> Router<AppState> {
    Router::new().route(
        "/api/admin/plugins/{plugin_id}/{*tail}",
        any(dispatch_handler),
    )
}

async fn dispatch_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((plugin_id, tail)): Path<(String, String)>,
    method: Method,
    Query(query): Query<BTreeMap<String, String>>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    require_controller(&state, &user)?;
    let path_with_slash = format!("/{tail}");

    // Look up the matching admin_route declaration.
    let routes = state.plugin_host.registry().admin_routes_for(&plugin_id);
    let upper = method.as_str().to_uppercase();
    let decl = routes
        .into_iter()
        .find(|r| r.method == upper && r.path == path_with_slash)
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "plugin_admin_route_not_found",
            message: format!(
                "no [[admin_routes]] entry on plugin '{plugin_id}' \
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

    // Decode body. Try JSON first; fall back to a raw string when
    // the body isn't JSON (rare but supported — e.g. multipart-like
    // bodies the plugin handles itself).
    let body_value: serde_json::Value = if body.is_empty() {
        serde_json::Value::Null
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => serde_json::Value::String(
                String::from_utf8_lossy(&body).to_string(),
            ),
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

    // Dispatch.
    use execlaw_script::primitives_glue::json_to_rhai;
    let dyn_args = vec![json_to_rhai(&args)];
    let result = plugin
        .invoke_async_owned(decl.handler.clone(), dyn_args)
        .await
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "plugin_admin_handler_error",
            message: format!("[{plugin_id}] handler {}: {e}", decl.handler),
        })?;
    Ok((StatusCode::OK, Json(result)).into_response())
}
