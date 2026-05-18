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
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use execlaw_plugin_host::PluginHostError;
use execlaw_plugin_sdk::zip_stage::{StageError, stage_zip};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::path::PathBuf;

/// `?if_existing=` query param on /install. Drives whether an
/// already-installed plugin id is rejected (default — safer; the
/// SPA catches the 409 and shows a replace-confirm dialog) or
/// upgraded in place (preserves OAuth client config + tokens).
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IfExisting {
    /// Return 409 when a plugin with the same id already exists.
    /// Default — keeps a stray re-upload from silently replacing
    /// a working plugin.
    #[default]
    Reject,
    /// Tear down the old runtime + state_plugins row, then install
    /// the new ZIP. Per-plugin OAuth client + token rows survive
    /// because they live in `state_oauth_*`, not `state_plugins`.
    Upgrade,
}

#[derive(Debug, Default, Deserialize)]
pub struct InstallQuery {
    #[serde(default)]
    pub if_existing: IfExisting,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PluginSummary {
    pub plugin_id: String,
    pub version: String,
    pub enabled: bool,
    pub installed_at: i64,
    pub updated_at: i64,
    /// True when the plugin's manifest declares any of the
    /// configurable hooks the SPA renders a settings page for —
    /// either `[[oauth_accounts]]` (existing Google plugins) or
    /// `[[ui_panels]]` (generic per-plugin React panel mount,
    /// used by Signal's QR pairing UI). The SPA shows a gear icon
    /// on rows where this is true, navigating to
    /// `/settings/plugins/{plugin_id}`. Schema-driven
    /// `[[settings_fields]]` lands later and flips this true for
    /// plugins with operator-editable knobs but no OAuth.
    pub has_settings_ui: bool,
    /// True when the plugin's manifest declares one or more
    /// `[[services]]` entries — sidecar containers the supervisor
    /// must spawn for the plugin to function. The SPA reads this on
    /// the plugin config page and, when Docker is unreachable
    /// (e.g. an Apple-Silicon install where the wizard skipped the
    /// Docker step), renders a "Docker required for this plugin"
    /// warning above the dynamic panel so the operator isn't left
    /// staring at a sidecar that never comes healthy. Plugins that
    /// declare zero services (Discord, open-meteo) keep this
    /// `false` and the warning never appears.
    pub has_sidecars: bool,
    /// Operator-facing one-liner from `[plugin].description`. The
    /// SPA renders this under the row title (single line, ellipsis
    /// truncated). `None` when the manifest omits the field or the
    /// stored manifest_toml is unparseable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 2026-05-15 — health surface for the SPA's row badge. Values:
    ///   * `"healthy"` (default) — plugin loaded cleanly on the last
    ///     hydrate / install / enable. SPA renders the row normally.
    ///   * `"quarantined"` — boot-time hydrate failed (e.g. staged
    ///     `main.rhai` missing, manifest unparseable, subprocess
    ///     spawn failed). The plugin host disabled the in-memory
    ///     hooks but kept the install record + OAuth + vault state
    ///     so a reinstall heals everything. SPA should render a
    ///     yellow "broken — needs reinstall" badge with
    ///     `health_message` in the tooltip and offer a re-upload
    ///     affordance instead of the normal config page (whose
    ///     admin routes would 404 because the registry is empty).
    pub health_status: String,
    /// One-line failure reason when `health_status != "healthy"`.
    /// Examples:
    ///   * `"script load failed: io: cannot find path"`
    ///   * `"manifest unparseable: missing [plugin] table"`
    ///   * `"subprocess spawn failed: ENOENT"`
    /// `None` when healthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_message: Option<String>,
    /// Unix timestamp (seconds) when the plugin entered its current
    /// non-healthy state. `None` when healthy. Lets the SPA render
    /// "broken since 17:21" — useful for distinguishing a fresh
    /// failure from a long-untouched one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quarantined_at: Option<i64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PluginListResponse {
    pub plugins: Vec<PluginSummary>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct InstallResponse {
    pub plugin_id: String,
    pub version: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ToolSummary {
    pub name: String,
    pub plugin_id: String,
    pub latency: String,
    pub required_capabilities: Vec<String>,
}

/// `POST /api/admin/plugins/install` — accepts a ZIP file in the
/// request body. Phase 2 uses raw `application/zip` bytes; switching
/// to multipart lands when the React upload form does.
#[utoipa::path(
    post,
    path = "/api/admin/plugins/install",
    request_body(content_type = "application/zip", description = "Plugin ZIP archive"),
    responses(
        (status = 200, description = "Installed", body = InstallResponse),
        (status = 400, description = "Invalid ZIP / manifest"),
        (status = 409, description = "Plugin already installed"),
    ),
    tag = "plugins"
)]
pub async fn install_handler(
    State(state): State<AppState>,
    Query(q): Query<InstallQuery>,
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
    // If the new ZIP is the same version as the install we're
    // about to replace, the stage path is identical to the old
    // one — that's fine and the host's `upgrade()` notices and
    // skips the "remove old stage dir" step. Reject only when the
    // operator is doing a fresh install (Reject mode) since that
    // would otherwise clobber an unrelated stage.
    if target.exists() && matches!(q.if_existing, IfExisting::Reject) {
        return error_response(
            StatusCode::CONFLICT,
            "already_staged",
            &format!("a staged dir already exists at {}", target.display()),
        );
    }
    // For an Upgrade where the same-version stage already exists,
    // tear it down so the rename below succeeds.
    if target.exists() && matches!(q.if_existing, IfExisting::Upgrade) {
        if let Err(e) = std::fs::remove_dir_all(&target) {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "stage_clear",
                &format!("could not clear existing stage dir: {e}"),
            );
        }
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

    // Drive install vs upgrade based on the operator's choice.
    let result = match q.if_existing {
        IfExisting::Reject => state.plugin_host.install(&target).await,
        IfExisting::Upgrade => {
            // Upgrade only makes sense if a row already exists; if
            // not, fall through to install so an "upgrade or
            // install" SPA flow works without two round-trips.
            let exists = state
                .plugin_host
                .list_rows()
                .ok()
                .and_then(|rows| {
                    rows.into_iter()
                        .find(|r| r.plugin_id == staged.manifest.plugin.id)
                        .map(|_| ())
                })
                .is_some();
            if exists {
                state.plugin_host.upgrade(&target).await
            } else {
                state.plugin_host.install(&target).await
            }
        }
    };
    let row = match result {
        Ok(r) => r,
        Err(e) => {
            // Best-effort cleanup of the staged dir on failure so we
            // don't leak orphans.
            let _ = std::fs::remove_dir_all(&target);
            return plugin_error_response(e);
        }
    };

    // Reflect the new tool surface into `config_tool_access`. Without
    // this, Settings → Tools (and the per-turn dispatch gate's policy
    // check) keeps serving the OLD tool list until the next server
    // restart hits the boot-time sync. For an upgrade, the OLD plugin
    // tools were removed from the registry by `host.upgrade()` — so we
    // mark them removed in the DB first, then re-sync the new set.
    // The order matters: mark-removed flips removed_at on every prior
    // row, then sync upserts the current set with removed_at=NULL,
    // leaving stale tools (e.g. one that 0.2 dropped) correctly tagged.
    sync_after_lifecycle_change(&state, &row.plugin_id);

    (
        StatusCode::OK,
        Json(serde_json::json!(InstallResponse {
            plugin_id: row.plugin_id,
            version: row.version,
        })),
    )
        .into_response()
}

/// Re-sync `config_tool_access` after a plugin lifecycle handler
/// mutated the in-memory registry. Best-effort: a sync failure is
/// logged but doesn't fail the operator's request — the worst case
/// is Settings → Tools renders stale until the next call (or next
/// server boot, which always runs the same sync).
fn sync_after_lifecycle_change(state: &AppState, plugin_id: &str) {
    let now = chrono::Utc::now().timestamp();
    if let Err(e) =
        crate::tool_sync::mark_plugin_tools_removed(&state.db, plugin_id, &state.plugin_host, now)
    {
        tracing::warn!(
            plugin_id = %plugin_id,
            error = %e,
            "mark_plugin_tools_removed failed during lifecycle sync",
        );
    }
    if let Err(e) = crate::tool_sync::sync_tool_access(&state.db, &state.plugin_host, now) {
        tracing::warn!(
            plugin_id = %plugin_id,
            error = %e,
            "sync_tool_access failed during lifecycle sync",
        );
    }
}

/// Predicate for `PluginSummary.has_settings_ui`. Pulled out so a
/// small unit test can pin the rule (a manifest with EITHER
/// `[[oauth_accounts]]` or `[[ui_panels]]` earns the gear icon)
/// without spinning the full DB-backed list_handler integration.
///
/// Pre-2026-05-05 we only checked `oauth_accounts`, so plugins
/// with a custom panel and no OAuth client (Signal's QR pairing UI)
/// had no gear icon and the operator had no obvious entry point.
fn manifest_has_settings_ui(m: &execlaw_plugin_sdk::PluginManifest) -> bool {
    !m.oauth_accounts.is_empty() || !m.ui_panels.is_empty()
}

/// Predicate for `PluginSummary.has_sidecars`. Pulled out as a tiny
/// helper so a unit test can pin "services declared ⇒ true" without
/// going through the full DB-backed list_handler integration. A
/// plugin declaring `[[services]]` is one the sidecar supervisor
/// must spawn through Docker — used by the SPA's plugin config
/// page to render a "Docker missing" warning when the daemon is
/// unreachable.
fn manifest_has_sidecars(m: &execlaw_plugin_sdk::PluginManifest) -> bool {
    !m.services.is_empty()
}

/// `GET /api/admin/plugins`
#[utoipa::path(
    get,
    path = "/api/admin/plugins",
    responses(
        (status = 200, description = "Installed plugin list", body = PluginListResponse),
    ),
    tag = "plugins"
)]
pub async fn list_handler(State(state): State<AppState>) -> impl IntoResponse {
    match state.plugin_host.list_rows() {
        Ok(rows) => {
            let plugins: Vec<PluginSummary> = rows
                .into_iter()
                .map(|r| {
                    // Re-parse the manifest_toml just to detect
                    // the hooks the settings UI surfaces. Cheap
                    // (small TOML) and the list endpoint is
                    // operator-tier polling cadence, not a hot
                    // path. If parsing fails (corrupt persisted
                    // row), default to no settings UI rather than
                    // failing the whole list.
                    let parsed = execlaw_plugin_sdk::PluginManifest::parse(&r.manifest_toml).ok();
                    let has_settings_ui = parsed
                        .as_ref()
                        .map(manifest_has_settings_ui)
                        .unwrap_or(false);
                    // Mirror `has_settings_ui`'s tolerance of an
                    // unparseable manifest: default to `false`
                    // (assume no sidecars) rather than failing the
                    // whole list. The operator will still see the
                    // row; the Docker-required warning just won't
                    // fire for this single broken plugin.
                    let has_sidecars = parsed
                        .as_ref()
                        .map(manifest_has_sidecars)
                        .unwrap_or(false);
                    let description = parsed
                        .as_ref()
                        .and_then(|m| m.plugin.description.clone())
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty());
                    PluginSummary {
                        plugin_id: r.plugin_id,
                        version: r.version,
                        enabled: r.enabled,
                        installed_at: r.installed_at,
                        updated_at: r.updated_at,
                        has_settings_ui,
                        has_sidecars,
                        description,
                        health_status: r.health_status,
                        health_message: r.health_message,
                        quarantined_at: r.quarantined_at,
                    }
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
#[utoipa::path(
    post,
    path = "/api/admin/plugins/{plugin_id}/enable",
    params(
        ("plugin_id" = String, Path, description = "Installed plugin id"),
    ),
    responses(
        (status = 200, description = "Plugin re-enabled"),
        (status = 404, description = "Plugin not installed"),
    ),
    tag = "plugins"
)]
pub async fn enable_handler(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
) -> impl IntoResponse {
    match state.plugin_host.enable(&plugin_id).await {
        Ok(()) => {
            // Re-enable re-registers hooks; the previous disable left
            // `removed_at` set on every owned row. Sync clears it.
            sync_after_lifecycle_change(&state, &plugin_id);
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Err(e) => plugin_error_response(e),
    }
}

/// `POST /api/admin/plugins/:id/disable`
#[utoipa::path(
    post,
    path = "/api/admin/plugins/{plugin_id}/disable",
    params(
        ("plugin_id" = String, Path, description = "Installed plugin id"),
    ),
    responses(
        (status = 200, description = "Plugin disabled"),
        (status = 404, description = "Plugin not installed"),
    ),
    tag = "plugins"
)]
pub async fn disable_handler(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
) -> impl IntoResponse {
    match state.plugin_host.disable(&plugin_id).await {
        Ok(()) => {
            // Disable removes hooks; mark every owned tool removed
            // so the dispatch gate stops accepting them.
            let now = chrono::Utc::now().timestamp();
            if let Err(e) = crate::tool_sync::mark_plugin_tools_removed(
                &state.db,
                &plugin_id,
                &state.plugin_host,
                now,
            ) {
                tracing::warn!(
                    plugin_id = %plugin_id,
                    error = %e,
                    "mark_plugin_tools_removed failed on disable",
                );
            }
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Err(e) => plugin_error_response(e),
    }
}

/// `DELETE /api/admin/plugins/:id`
#[utoipa::path(
    delete,
    path = "/api/admin/plugins/{plugin_id}",
    params(
        ("plugin_id" = String, Path, description = "Installed plugin id"),
    ),
    responses(
        (status = 200, description = "Plugin uninstalled"),
        (status = 404, description = "Plugin not installed"),
    ),
    tag = "plugins"
)]
pub async fn uninstall_handler(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
) -> impl IntoResponse {
    // 2026-05-14 — switched from `plugin_host.uninstall` (DB-row +
    // stage-dir only) to the full lifecycle `purge_plugin`. The
    // previous path left orphan sidecar containers (WhatsApp
    // wuzapi survived reinstall and refused to start because its
    // container name + port were already taken), orphan OAuth
    // grants, orphan vault secrets, and orphan artifact blobs.
    // `purge_plugin` runs the full ordered teardown documented in
    // `plugin_lifecycle` — fire on_disable → stop+remove sidecar
    // containers → rm state dirs → delete OAuth/vault/artifact
    // rows → uninstall (archive skills + delete state_plugins row +
    // rm stage dir). The tool-sync `mark_plugin_tools_removed` is
    // baked into `purge_plugin` after its uninstall step, so the
    // SPA tools panel + dispatch gate still reflect the removal.
    let report = crate::plugin_lifecycle::purge_plugin(&state, &plugin_id).await;
    // The lifecycle orchestrator is best-effort: a missing plugin
    // (already uninstalled) returns `uninstalled = false` with no
    // errors and that's a valid idempotent 200. A true failure
    // surfaces in `report.errors`; we still return 200 with the
    // report so the SPA can show partial-teardown context, but log
    // any error entries at WARN for ops visibility.
    if !report.errors.is_empty() {
        tracing::warn!(
            target: "plugins::uninstall",
            plugin_id = %plugin_id,
            errors = ?report.errors,
            "plugin uninstall completed with errors",
        );
    }
    (StatusCode::OK, Json(report)).into_response()
}

/// `GET /api/admin/plugins/tools` — union of all live plugin tools.
#[utoipa::path(
    get,
    path = "/api/admin/plugins/tools",
    responses(
        (status = 200, description = "Every plugin-contributed tool the agent can call"),
    ),
    tag = "plugins"
)]
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
    (StatusCode::OK, Json(serde_json::json!({"tools": tools}))).into_response()
}

/// One sidebar-nav entry the SPA renders under `⋯ More`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
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

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct UiPanelListResponse {
    pub panels: Vec<UiPanelSummary>,
}

/// `GET /api/admin/plugins/ui_panels` — list every installed plugin's
/// declared UI panels, in deterministic order (by mount path) so the
/// sidebar nav doesn't reshuffle on every refresh.
///
/// Trusted-plugin model: the SPA loads `entry` via dynamic ESM import
/// with no sandboxing. Install was already gated by controller auth.
#[utoipa::path(
    get,
    path = "/api/admin/plugins/ui_panels",
    responses(
        (status = 200, description = "Sidebar panel manifests, sorted by mount path", body = UiPanelListResponse),
    ),
    tag = "plugins"
)]
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

    /// `manifest_has_settings_ui` decides whether the plugins-list
    /// row gets a gear icon. Either `[[oauth_accounts]]` OR
    /// `[[ui_panels]]` qualifies — Signal lands as a `[[ui_panels]]`-
    /// only plugin and must light up the gear without an OAuth
    /// client (which it doesn't have).
    #[test]
    fn manifest_has_settings_ui_true_for_oauth_or_panels() {
        let oauth_only = execlaw_plugin_sdk::PluginManifest::parse(
            r#"
[plugin]
id = "oauth-plugin"
name = "OAuth Plugin"
version = "1.0.0"

[[oauth_accounts]]
name = "controller"
provider = "google"
"#,
        )
        .unwrap();
        assert!(manifest_has_settings_ui(&oauth_only));

        let panels_only = execlaw_plugin_sdk::PluginManifest::parse(
            r#"
[plugin]
id = "signal"
name = "Signal"
version = "0.2.0"

[[ui_panels]]
mount = "admin/plugins/signal"
entry = "ui/panel.js"
"#,
        )
        .unwrap();
        assert!(
            manifest_has_settings_ui(&panels_only),
            "ui_panels-only plugins (e.g. Signal) must earn the gear icon",
        );

        let neither = execlaw_plugin_sdk::PluginManifest::parse(
            r#"
[plugin]
id = "tools-only"
name = "Tools only"
version = "1.0.0"

[[tools]]
name = "noop"
latency = "low"
"#,
        )
        .unwrap();
        assert!(!manifest_has_settings_ui(&neither));
    }

    /// Pin the `manifest_has_sidecars` rule: a manifest with any
    /// `[[services]]` entry must report `true`, and manifests
    /// without one must report `false`. The Docker-missing warning
    /// on the plugin config page reads this field, so a regression
    /// here either spams the warning for sidecar-free plugins
    /// (false positive) or silences it for plugins that genuinely
    /// can't run (false negative — operator sees a stuck plugin
    /// with no signal why).
    #[test]
    fn manifest_has_sidecars_true_when_services_declared() {
        let with_services = execlaw_plugin_sdk::PluginManifest::parse(
            r#"
[plugin]
id = "python-sandbox"
name = "Python Sandbox"
version = "0.1.0"

[[services]]
name = "kernel-gateway"
image = "execlaw/python-sandbox-fast:0.1.0"

[services.sidecar]
rpc_port = 8888
health_path = "/api/kernels"
"#,
        )
        .unwrap();
        assert!(
            manifest_has_sidecars(&with_services),
            "plugins declaring [[services]] must trigger the Docker warning",
        );

        let no_services = execlaw_plugin_sdk::PluginManifest::parse(
            r#"
[plugin]
id = "open-meteo"
name = "Open-Meteo"
version = "0.5.0"

[[tools]]
name = "open_meteo.current"
latency = "low"
"#,
        )
        .unwrap();
        assert!(
            !manifest_has_sidecars(&no_services),
            "plugins without [[services]] must NOT trigger the Docker warning",
        );
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
