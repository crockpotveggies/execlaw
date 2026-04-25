//! Plugin admin routes (Phase 2, §4).
//!
//! - `POST   /api/admin/plugins/install` — upload a ZIP, stage, validate
//!   manifest, register hooks, spawn subprocess, persist.
//! - `GET    /api/admin/plugins` — list installed plugins.
//! - `POST   /api/admin/plugins/:id/enable` — re-register hooks + respawn.
//! - `POST   /api/admin/plugins/:id/disable` — un-register + kill child.
//! - `DELETE /api/admin/plugins/:id` — full uninstall.
//! - `GET    /api/admin/plugins/tools` — list every tool the agent can
//!   currently call (union of built-ins + plugin-contributed tools).
//!
//! Install uses the multipart form field `file` for the ZIP. For
//! Phase 2 we accept raw `application/zip` bytes too — see
//! [`install_handler`].

use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router};
use axum::routing::{delete, get, post};
use execlaw_plugin_host::PluginHostError;
use execlaw_plugin_sdk::zip_stage::{StageError, stage_zip};
use serde::Serialize;
use std::io::Cursor;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
pub struct PluginSummary {
    pub plugin_id: String,
    pub version: String,
    pub enabled: bool,
    pub installed_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Serialize)]
pub struct PluginListResponse {
    pub plugins: Vec<PluginSummary>,
}

#[derive(Debug, Serialize)]
pub struct InstallResponse {
    pub plugin_id: String,
    pub version: String,
}

#[derive(Debug, Serialize)]
pub struct ToolSummary {
    pub name: String,
    pub plugin_id: String,
    pub latency: String,
    pub required_capabilities: Vec<String>,
}

/// `POST /api/admin/plugins/install` — accepts a ZIP file in the
/// request body. Phase 2 uses raw `application/zip` bytes; switching
/// to multipart lands when the React upload form does.
pub async fn install_handler(
    State(state): State<AppState>,
    body: Bytes,
) -> impl IntoResponse {
    if body.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "empty_body",
            "request body is empty — upload a ZIP",
        );
    }

    // Stage to a temp dir (existing plugin-sdk helper), then move to
    // a stable location under <stage_root>/<plugin_id>-<version>/ so
    // the install persists across restarts.
    let staged = match stage_zip(Cursor::new(&body[..])) {
        Ok(s) => s,
        Err(e) => {
            let (code, msg) = match &e {
                StageError::MissingManifest => (StatusCode::BAD_REQUEST, "missing manifest"),
                _ => (StatusCode::BAD_REQUEST, "stage failed"),
            };
            return error_response(code, "stage_failed", &format!("{msg}: {e}"));
        }
    };

    let target: PathBuf = state.plugin_host.stage_root().join(format!(
        "{}-{}",
        staged.manifest.plugin.id, staged.manifest.plugin.version
    ));
    if target.exists() {
        return error_response(
            StatusCode::CONFLICT,
            "already_staged",
            &format!("a staged dir already exists at {}", target.display()),
        );
    }
    if let Err(e) = std::fs::create_dir_all(target.parent().unwrap()) {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "stage_mkdir",
            &format!("mkdir: {e}"),
        );
    }
    // Move the tempdir into place. `TempDir::into_path` releases the
    // auto-cleanup; we then rename the released path to the target.
    let released = staged.tempdir.keep();
    if let Err(e) = std::fs::rename(&released, &target) {
        // Fall back to copy-then-remove if rename cross-device-fails
        // (common on WSL/Windows with mounted volumes).
        if let Err(e2) = copy_dir_recursive(&released, &target) {
            let _ = std::fs::remove_dir_all(&released);
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "stage_move",
                &format!("rename+copy failed: {e} / {e2}"),
            );
        }
        let _ = std::fs::remove_dir_all(&released);
    }

    // Install into the host (parses manifest, registers hooks,
    // spawns subprocess, persists row).
    let row = match state.plugin_host.install(&target).await {
        Ok(r) => r,
        Err(e) => {
            // Best-effort cleanup of the staged dir on failure so we
            // don't leak orphans.
            let _ = std::fs::remove_dir_all(&target);
            return plugin_error_response(e);
        }
    };

    (
        StatusCode::OK,
        Json(serde_json::json!(InstallResponse {
            plugin_id: row.plugin_id,
            version: row.version,
        })),
    )
        .into_response()
}

/// `GET /api/admin/plugins`
pub async fn list_handler(State(state): State<AppState>) -> impl IntoResponse {
    match state.plugin_host.list_rows() {
        Ok(rows) => {
            let plugins: Vec<PluginSummary> = rows
                .into_iter()
                .map(|r| PluginSummary {
                    plugin_id: r.plugin_id,
                    version: r.version,
                    enabled: r.enabled,
                    installed_at: r.installed_at,
                    updated_at: r.updated_at,
                })
                .collect();
            (
                StatusCode::OK,
                Json(serde_json::json!(PluginListResponse { plugins })),
            )
                .into_response()
        }
        Err(e) => plugin_error_response(e),
    }
}

/// `POST /api/admin/plugins/:id/enable`
pub async fn enable_handler(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
) -> impl IntoResponse {
    match state.plugin_host.enable(&plugin_id).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => plugin_error_response(e),
    }
}

/// `POST /api/admin/plugins/:id/disable`
pub async fn disable_handler(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
) -> impl IntoResponse {
    match state.plugin_host.disable(&plugin_id).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => plugin_error_response(e),
    }
}

/// `DELETE /api/admin/plugins/:id`
pub async fn uninstall_handler(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
) -> impl IntoResponse {
    match state.plugin_host.uninstall(&plugin_id).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => plugin_error_response(e),
    }
}

/// `GET /api/admin/plugins/tools` — union of all live plugin tools.
pub async fn list_tools_handler(State(state): State<AppState>) -> impl IntoResponse {
    let tools: Vec<ToolSummary> = state
        .plugin_host
        .registry()
        .all_tools()
        .into_iter()
        .map(|t| ToolSummary {
            name: t.tool_name.clone(),
            plugin_id: t.plugin_id.clone(),
            latency: t.latency.clone(),
            required_capabilities: t.required_capabilities.clone(),
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"tools": tools})),
    )
        .into_response()
}

/// One sidebar-nav entry the SPA renders under `⋯ More`.
#[derive(Debug, Serialize)]
pub struct UiPanelSummary {
    pub plugin_id: String,
    /// URL path segment the SPA mounts the panel at, e.g.
    /// `admin/plugins/google-calendar`. The SPA prepends its own
    /// router base.
    pub mount: String,
    /// Path inside the plugin bundle to the panel's entry module
    /// (relative to the plugin's static-asset root).
    pub entry: String,
}

#[derive(Debug, Serialize)]
pub struct UiPanelListResponse {
    pub panels: Vec<UiPanelSummary>,
}

/// `GET /api/admin/plugins/ui_panels` — list every installed plugin's
/// declared UI panels, in deterministic order (by mount path) so the
/// sidebar nav doesn't reshuffle on every refresh.
///
/// Trusted-plugin model: the SPA loads `entry` via dynamic ESM import
/// with no sandboxing. Install was already gated by controller auth.
pub async fn list_ui_panels_handler(State(state): State<AppState>) -> impl IntoResponse {
    let mut panels: Vec<UiPanelSummary> = state
        .plugin_host
        .registry()
        .ui_panels()
        .into_iter()
        .map(|p| UiPanelSummary {
            plugin_id: p.plugin_id,
            mount: p.mount,
            entry: p.entry,
        })
        .collect();
    panels.sort_by(|a, b| a.mount.cmp(&b.mount));
    (
        StatusCode::OK,
        Json(serde_json::json!(UiPanelListResponse { panels })),
    )
        .into_response()
}

/// Sub-router mounted at `/api/admin/plugins/...`.
pub fn plugins_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/plugins", get(list_handler))
        .route("/api/admin/plugins/install", post(install_handler))
        .route("/api/admin/plugins/tools", get(list_tools_handler))
        .route("/api/admin/plugins/ui_panels", get(list_ui_panels_handler))
        .route(
            "/api/admin/plugins/{plugin_id}/enable",
            post(enable_handler),
        )
        .route(
            "/api/admin/plugins/{plugin_id}/disable",
            post(disable_handler),
        )
        .route("/api/admin/plugins/{plugin_id}", delete(uninstall_handler))
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Recursive directory copy for the cross-device-rename fallback.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn error_response(status: StatusCode, code: &str, message: &str) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({
            "error": {"code": code, "message": message}
        })),
    )
        .into_response()
}

fn plugin_error_response(e: PluginHostError) -> axum::response::Response {
    let (status, code) = match &e {
        PluginHostError::NotInstalled(_) => (StatusCode::NOT_FOUND, "not_installed"),
        PluginHostError::AlreadyInstalled(_) => (StatusCode::CONFLICT, "already_installed"),
        PluginHostError::HookConflict(_) => (StatusCode::CONFLICT, "hook_conflict"),
        PluginHostError::Manifest(_) => (StatusCode::BAD_REQUEST, "bad_manifest"),
        PluginHostError::UnsupportedTier(_) => (StatusCode::BAD_REQUEST, "unsupported_tier"),
        PluginHostError::MissingRuntime => (StatusCode::BAD_REQUEST, "missing_runtime"),
        PluginHostError::Spawn(_) => (StatusCode::INTERNAL_SERVER_ERROR, "spawn_failed"),
        PluginHostError::Db(_) | PluginHostError::Io(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    };
    error_response(status, code, &e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{self, Body};
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    fn build_app() -> axum::Router {
        crate::routes::build_router(crate::routes::test_app_state())
    }

    async fn read_json(resp: axum::response::Response) -> (StatusCode, serde_json::Value) {
        let status = resp.status();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    /// With no plugins installed, the route returns an empty list (200,
    /// not 404).
    #[tokio::test]
    async fn ui_panels_empty_when_no_plugins() {
        let app = build_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/admin/plugins/ui_panels")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = read_json(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["panels"].is_array());
        assert_eq!(body["panels"].as_array().unwrap().len(), 0);
    }

    /// Two plugins each declaring a panel show up sorted by mount path,
    /// regardless of install order.
    #[tokio::test]
    async fn ui_panels_returns_all_registered_panels_sorted() {
        let state = crate::routes::test_app_state();

        // Install plugin "z-thing" FIRST so its panel would naturally
        // appear earlier in registration order — sort by mount must
        // override that.
        let z = r#"
[plugin]
id = "z-thing"
name = "Z thing"
version = "1.0.0"

[[ui_panels]]
mount = "admin/plugins/z-thing"
entry = "ui/z.js"
"#;
        let a = r#"
[plugin]
id = "a-thing"
name = "A thing"
version = "1.0.0"

[[ui_panels]]
mount = "admin/plugins/a-thing"
entry = "ui/a.js"
"#;
        state
            .plugin_host
            .registry()
            .enable(&execlaw_plugin_sdk::PluginManifest::parse(z).unwrap())
            .unwrap();
        state
            .plugin_host
            .registry()
            .enable(&execlaw_plugin_sdk::PluginManifest::parse(a).unwrap())
            .unwrap();

        let app = crate::routes::build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/admin/plugins/ui_panels")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = read_json(resp).await;
        assert_eq!(status, StatusCode::OK);
        let panels = body["panels"].as_array().unwrap();
        assert_eq!(panels.len(), 2);
        // Sorted by mount path → a-thing first, z-thing second.
        assert_eq!(panels[0]["plugin_id"], "a-thing");
        assert_eq!(panels[0]["mount"], "admin/plugins/a-thing");
        assert_eq!(panels[0]["entry"], "ui/a.js");
        assert_eq!(panels[1]["plugin_id"], "z-thing");
    }

    /// A plugin with no `[[ui_panels]]` blocks contributes nothing — the
    /// presence of other plugin features must not leak into this route.
    #[tokio::test]
    async fn ui_panels_excludes_plugins_without_panels() {
        let state = crate::routes::test_app_state();
        let m = r#"
[plugin]
id = "tools-only"
name = "Tools only"
version = "1.0.0"

[[tools]]
name = "noop"
schema = "s.json"
latency = "low"
required_capabilities = []
"#;
        state
            .plugin_host
            .registry()
            .enable(&execlaw_plugin_sdk::PluginManifest::parse(m).unwrap())
            .unwrap();
        let app = crate::routes::build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/admin/plugins/ui_panels")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = read_json(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["panels"].as_array().unwrap().len(), 0);
    }
}
