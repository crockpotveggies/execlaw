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
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    require_controller(&state, &user)?;

    // UI-asset short-circuit. Anything under `/ui/...` is a request
    // for a file from the plugin's staged directory (panel.js,
    // panel.css, image assets, …). This is how the SPA dynamically
    // loads each plugin's self-contained config panel without
    // baking plugin-specific code into the host bundle.
    //
    // The lookup is path-traversal guarded: we canonicalise both the
    // plugin's stage root and the resolved asset path, and reject
    // anything that escapes the stage. Auth is already enforced by
    // `require_controller` above.
    //
    // The SPA loads the asset via authenticated `fetch()` and turns
    // the response into a `URL.createObjectURL()` blob before
    // `import()`-ing it — the import primitive doesn't carry the
    // bearer token, so we serve via a normal request and let the
    // browser's blob loader handle the module instantiation.
    if method == Method::GET && tail.starts_with("ui/") {
        return serve_plugin_ui_asset(&state, &plugin_id, &tail, &headers).await;
    }

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
            Err(_) => serde_json::Value::String(String::from_utf8_lossy(&body).to_string()),
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

/// Serve a static file from a plugin's staged directory.
///
/// The plugin's UI panel ships as part of its ZIP (alongside
/// `plugin.toml` + `main.rhai`), under `ui/`. The SPA's
/// `DynamicPluginPanel` component fetches `ui/panel.js` (and any
/// sibling assets — CSS, source maps) via authenticated `fetch()`,
/// constructs a Blob URL, and dynamic-`import()`s it. This keeps
/// every plugin's frontend code inside its own ZIP — no
/// plugin-specific imports bleed into the host SPA bundle.
///
/// **Security:** we canonicalise both the plugin's stage root AND
/// the resolved asset path, then assert the asset path starts with
/// the stage root. A `../../../etc/passwd`-style tail is rejected
/// before any file open. Auth is enforced upstream
/// (`require_controller` in the caller).
///
/// **Caching:** weak ETag derived from `(mtime_ms, size_bytes)`.
/// If the SPA sends `If-None-Match` matching the current ETag we
/// return `304 Not Modified` without re-reading the file — useful
/// for the SPA's panel-load pattern which fetches the same
/// `ui/panel.js` repeatedly during a session as the operator
/// navigates around. Plugin upgrades shift mtime, invalidating the
/// cache.
async fn serve_plugin_ui_asset(
    state: &AppState,
    plugin_id: &str,
    tail: &str,
    request_headers: &axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    use axum::http::header;

    let row = state
        .plugin_host
        .get_row(plugin_id)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "db_error",
            message: e.to_string(),
        })?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "plugin_not_installed",
            message: format!("plugin '{plugin_id}' is not installed"),
        })?;

    // Canonicalise the stage root so the symlink-aware comparison
    // below survives operators who put `~/.execlaw/plugins` behind
    // a symlink. `canonicalize` is the right primitive — `fs::canon`
    // doesn't exist; symlink resolution is what we need.
    let stage_root_raw = std::path::PathBuf::from(&row.stage_path);
    let stage_root_canon = stage_root_raw.canonicalize().map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "stage_root_unreadable",
        message: format!("stage_path '{}' unreadable: {e}", row.stage_path),
    })?;

    // Build the candidate asset path. `tail` already begins with
    // `ui/`; join it onto the stage root.
    let candidate = stage_root_canon.join(tail);
    // Canonicalise the candidate (this is also what blocks
    // `..` traversal: the canonical form of `<stage>/ui/../../x` is
    // somewhere outside `<stage>` if the segments resolve up enough,
    // and the prefix check below catches it).
    let candidate_canon = match candidate.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return Err(ApiError {
                status: StatusCode::NOT_FOUND,
                code: "ui_asset_not_found",
                message: format!("plugin '{plugin_id}' has no asset at '{tail}'"),
            });
        }
    };
    if !candidate_canon.starts_with(&stage_root_canon) {
        // Path-traversal attempt: asset resolved outside the plugin's
        // stage. Refuse with 404 (rather than 403) so we don't leak
        // whether the target exists.
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "ui_asset_not_found",
            message: format!("plugin '{plugin_id}' has no asset at '{tail}'"),
        });
    }

    // Compute the ETag from mtime + size. Weak (`W/"..."`) because
    // we don't compare bytes — two files with identical (mtime,
    // size) could in theory differ, but for our plugin-zip workflow
    // any content change comes with a fresh extract that bumps
    // mtime.
    let meta = std::fs::metadata(&candidate_canon).map_err(|_| ApiError {
        status: StatusCode::NOT_FOUND,
        code: "ui_asset_not_found",
        message: format!("plugin '{plugin_id}' has no asset at '{tail}'"),
    })?;
    if !meta.is_file() {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "ui_asset_not_found",
            message: format!("plugin '{plugin_id}' has no asset at '{tail}'"),
        });
    }
    let mtime_ms: i128 = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i128)
        .unwrap_or(0);
    let size = meta.len();
    let etag = format!("W/\"{plugin_id}-{mtime_ms}-{size}\"");

    if let Some(inm) = request_headers.get(header::IF_NONE_MATCH) {
        if let Ok(inm_str) = inm.to_str() {
            if inm_str.split(',').any(|t| t.trim() == etag) {
                let mut resp = Response::new(axum::body::Body::empty());
                *resp.status_mut() = StatusCode::NOT_MODIFIED;
                resp.headers_mut()
                    .insert(header::ETAG, etag.parse().unwrap());
                return Ok(resp);
            }
        }
    }

    let bytes = std::fs::read(&candidate_canon).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "ui_asset_read_failed",
        message: e.to_string(),
    })?;
    let content_type = content_type_for(&candidate_canon);
    let mut resp = Response::new(axum::body::Body::from(bytes));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        content_type
            .parse()
            .unwrap_or_else(|_| "application/octet-stream".parse().unwrap()),
    );
    resp.headers_mut()
        .insert(header::ETAG, etag.parse().unwrap());
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        "private, max-age=0, must-revalidate".parse().unwrap(),
    );
    Ok(resp)
}

/// Map a path extension to a `Content-Type` header. Small allowlist
/// covering the file kinds plugin UI panels actually ship; anything
/// unrecognised falls back to `application/octet-stream` (browser
/// will still serve binary, just won't render).
fn content_type_for(path: &std::path::Path) -> &'static str {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name.ends_with(".js") || name.ends_with(".mjs") {
        "application/javascript; charset=utf-8"
    } else if name.ends_with(".js.map") || name.ends_with(".json") {
        "application/json; charset=utf-8"
    } else if name.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if name.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if name.ends_with(".svg") {
        "image/svg+xml"
    } else if name.ends_with(".png") {
        "image/png"
    } else if name.ends_with(".jpg") || name.ends_with(".jpeg") {
        "image/jpeg"
    } else if name.ends_with(".webp") {
        "image/webp"
    } else if name.ends_with(".woff2") {
        "font/woff2"
    } else if name.ends_with(".woff") {
        "font/woff"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::{self, Body};
    use axum::http::{Request, header};
    use execlaw_plugin_sdk::PluginManifest;
    use std::fs;
    use std::path::PathBuf;
    use tower::ServiceExt;

    /// Helper: install a fake plugin into the test app state and
    /// drop a `ui/panel.js` file into its stage dir. Returns the
    /// auth token + the stage path so the test can drop more files.
    async fn fixture_with_panel(
        app: &axum::Router,
        state: &AppState,
        plugin_id: &str,
        panel_body: &[u8],
    ) -> (String, PathBuf) {
        // Mint controller token.
        let body = serde_json::to_vec(&serde_json::json!({
            "username": "ctrl",
            "admin_password": "hunter2-longer",
            "display_name": "Ctrl",
        }))
        .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/setup")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let tok = v["access_token"].as_str().unwrap().to_owned();

        // Stage a fake plugin directory under the host's stage_root
        // and register it in the DB the way `install` would. The
        // simplest path is to write the files + call the host's
        // install pipeline directly.
        let stage = state
            .plugin_host
            .stage_root()
            .join(format!("{plugin_id}-0.1.0"));
        fs::create_dir_all(stage.join("ui")).unwrap();
        fs::write(stage.join("ui").join("panel.js"), panel_body).unwrap();
        let manifest = format!(
            r#"
[plugin]
id = "{plugin_id}"
name = "Fixture"
version = "0.1.0"

[[ui_panels]]
mount = "admin/plugins/{plugin_id}"
entry = "ui/panel.js"
"#
        );
        fs::write(stage.join("plugin.toml"), &manifest).unwrap();
        // Parse + register via the host so `state_plugins` has a row
        // pointing at the staged dir.
        let _parsed = PluginManifest::parse(&manifest).unwrap();
        state.plugin_host.install(&stage).await.unwrap();
        (tok, stage)
    }

    #[tokio::test]
    async fn ui_asset_serves_panel_js_with_javascript_content_type() {
        let state = test_app_state();
        let app = build_router(state.clone());
        let panel_body = b"export default function panel() {}";
        let (tok, _stage) = fixture_with_panel(&app, &state, "fixture-ui", panel_body).await;

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/plugins/fixture-ui/ui/panel.js")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/javascript; charset=utf-8"),
        );
        assert!(resp.headers().get(header::ETAG).is_some());
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(bytes.as_ref(), panel_body);
    }

    #[tokio::test]
    async fn ui_asset_returns_304_on_matching_if_none_match() {
        let state = test_app_state();
        let app = build_router(state.clone());
        let (tok, _stage) = fixture_with_panel(&app, &state, "fixture-etag", b"x").await;

        // First request — capture the ETag.
        let req1 = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/plugins/fixture-etag/ui/panel.js")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp1 = app.clone().oneshot(req1).await.unwrap();
        let etag = resp1
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .expect("ETag must be present on 200")
            .to_owned();

        // Second request with If-None-Match — expect 304.
        let req2 = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/plugins/fixture-etag/ui/panel.js")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::IF_NONE_MATCH, &etag)
            .body(Body::empty())
            .unwrap();
        let resp2 = app.oneshot(req2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::NOT_MODIFIED);
        let bytes = body::to_bytes(resp2.into_body(), usize::MAX).await.unwrap();
        assert!(bytes.is_empty(), "304 must have empty body");
    }

    #[tokio::test]
    async fn ui_asset_rejects_path_traversal() {
        let state = test_app_state();
        let app = build_router(state.clone());
        let (tok, _stage) = fixture_with_panel(&app, &state, "fixture-traversal", b"x").await;

        // Attempt to escape the stage directory via `..` segments.
        // The route requires the tail to start with `ui/`, so we
        // build something like `ui/../../etc/hosts`. The canonical
        // form of that path is OUTSIDE the stage root — the prefix
        // check must catch it and return 404.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/plugins/fixture-traversal/ui/../../../../../etc/hosts")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // The router normalises `..` before reaching the handler in
        // some setups, so we'll see either 404 (handler caught it)
        // or 404 (router rewrote it out of `/ui/`). Both are
        // acceptable; the invariant is "never 200 leaking external
        // file contents".
        assert!(
            resp.status() == StatusCode::NOT_FOUND || resp.status() == StatusCode::BAD_REQUEST,
            "path-traversal attempt must NOT succeed; got {}",
            resp.status(),
        );
    }

    #[tokio::test]
    async fn ui_asset_404s_for_unknown_plugin() {
        let state = test_app_state();
        let app = build_router(state.clone());
        // No plugin installed; setup first to mint a token.
        let body = serde_json::to_vec(&serde_json::json!({
            "username": "ctrl",
            "admin_password": "hunter2-longer",
            "display_name": "Ctrl",
        }))
        .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/setup")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let tok = v["access_token"].as_str().unwrap().to_owned();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/plugins/no-such-plugin/ui/panel.js")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn ui_asset_404s_for_missing_file_under_installed_plugin() {
        let state = test_app_state();
        let app = build_router(state.clone());
        let (tok, _stage) = fixture_with_panel(&app, &state, "fixture-missing", b"x").await;

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/plugins/fixture-missing/ui/nonexistent.js")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn content_type_for_known_extensions() {
        assert_eq!(
            content_type_for(std::path::Path::new("panel.js")),
            "application/javascript; charset=utf-8",
        );
        assert_eq!(
            content_type_for(std::path::Path::new("panel.mjs")),
            "application/javascript; charset=utf-8",
        );
        assert_eq!(
            content_type_for(std::path::Path::new("panel.css")),
            "text/css; charset=utf-8",
        );
        assert_eq!(
            content_type_for(std::path::Path::new("icon.svg")),
            "image/svg+xml",
        );
        assert_eq!(
            content_type_for(std::path::Path::new("unknown.xyz")),
            "application/octet-stream",
        );
    }
}
