//! Factory reset — wipe every user-data table back to first-boot state.
//!
//! Surfaced as the "Danger zone" at the bottom of Settings → General. The
//! operator types a literal confirmation string ("RESET") into the SPA;
//! the SPA POSTs it here, the handler purges every installed plugin's
//! resources (containers, state dirs, OAuth, vault, artifacts, stage
//! dirs), sweeps any remaining orphan filesystem state, then deletes
//! and re-creates the SQLite database file.
//!
//! Teardown ordering (2026-05-14 rework — now per-plugin lifecycle):
//!
//!   1. **`purge_all_plugins`** — for every installed plugin, run the
//!      full `purge` lifecycle (`plugin_lifecycle::purge_plugin`):
//!
//!        a. `PluginHost::disable` (fires `on_disable` hook while the
//!           plugin still has access to its OAuth tokens + transport
//!           bindings — lets a well-behaved plugin send a final
//!           "going offline" message, revoke an upstream OAuth grant).
//!        b. `SidecarSupervisor::remove_for_plugin` (stop + docker
//!           rm -f every container the plugin owns AND `rm -rf` its
//!           `~/.execlaw/sidecars/<plugin_id>/` state root).
//!        c. Delete plugin-scoped DB rows: `state_oauth_tokens`,
//!           `state_oauth_clients`, `state_artifacts`, `vault_secrets`
//!           (with refcount-aware blob deletion for artifacts).
//!        d. `PluginHost::uninstall` (archive skills, delete the
//!           `state_plugins` row, remove the staged plugin dir).
//!
//!      This is the load-bearing step that closes the
//!      "WhatsApp/Signal container + state survives factory reset"
//!      class of bugs. The earlier `stop_all` path only stopped
//!      containers; it didn't touch `~/.execlaw/sidecars/`, leaving
//!      signal-cli's keystore + wuzapi's session DB in place for the
//!      next install to silently inherit.
//!
//!   2. **Orphan-directory sweep** — `rm -rf` the on-disk directories
//!      that no plugin claimed: `~/.execlaw/sidecars/`,
//!      `~/.execlaw/plugin_artifacts/`, `~/.execlaw/plugins/`,
//!      `~/.execlaw/research/`. Catches anything the per-plugin
//!      purges missed (sidecar dir for a plugin whose state_plugins
//!      row was hand-edited away, research workspaces from cancelled
//!      jobs, partial uploads, etc.).
//!
//!   3. **DB rebuild** — `Database::rebuild_to_empty` closes the
//!      connection, deletes the `.db` + `-wal` + `-shm` + `-journal`
//!      files, opens a fresh empty file at the same path with the
//!      same encryption posture. Then `MigrationRunner::apply_all`
//!      re-creates schema + re-fires every migration-seeded singleton
//!      (config_general, config_personality, config_research, ...).
//!
//! Scope:
//!
//!   * In-memory caches (refresh tokens, plugin host registry, runner /
//!     backend supervisors, mcp host, voice sessions) survive the
//!     reset; the operator should restart the host service for full
//!     hygiene. The SPA surfaces this in the post-reset toast.
//!   * Sidecar Docker *images* (bbernhard/signal-cli-rest-api, …) are
//!     left in the local Docker cache — re-pulling on next install
//!     would waste bandwidth for no security benefit.
//!
//! The endpoint is Controller-only and idempotent — calling it twice
//! is harmless. The first call destroys the caller's session (the
//! `users` row backing their JWT is gone), so the SPA must sign-out
//! immediately on the 200 response and route to /login, where the
//! AppBoot guard will detect the missing controller and bounce to
//! /setup.

use crate::auth_extract::AuthedUser;
use crate::plugin_lifecycle::{PluginPurgeReport, purge_all_plugins};
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::post;
use execlaw_core::users::{UserRole, UserStore};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Literal string the operator must type into the danger-zone input.
/// Kept short and obvious so a non-English operator can still copy it
/// from the on-screen prompt; not a security boundary on its own —
/// the Controller-role check is what actually gates the call.
const CONFIRM_TOKEN: &str = "RESET";

#[derive(Debug, Deserialize, ToSchema)]
pub struct FactoryResetRequest {
    /// Must equal the literal string `"RESET"`. Anything else is a 400.
    pub confirm: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FactoryResetResponse {
    /// Number of tables present in the rebuilt DB. Matches the count
    /// of `CREATE TABLE` statements the migration set runs; equivalent
    /// to "every table the migration set declares."
    pub tables_wiped: usize,
    /// Number of migrations re-applied after the rebuild. On a healthy
    /// install this equals the length of the embedded migration set
    /// (currently 2: baseline + plugin_artifacts). A different count
    /// here means either a partial migration history or a future
    /// addition not yet run — log it loudly.
    #[serde(default)]
    pub migrations_reapplied: usize,
    /// Per-plugin teardown reports — one entry per installed plugin.
    /// Carries containers removed, on-disk state cleared, OAuth/vault
    /// rows deleted, etc. so the SPA can show "wiped these N plugins:
    /// signal (1 container, 12.4 MB), whatsapp (1 container, 4.1 MB)"
    /// instead of just a count.
    pub plugins_purged: Vec<PluginPurgeReport>,
    /// On-disk directory paths the sweep step recursively removed (paths
    /// that no plugin claimed: research workspaces from cancelled jobs,
    /// stage dirs for plugins with hand-edited DB rows, etc.).
    pub orphan_dirs_removed: Vec<String>,
    /// Operator-facing reminder — the in-memory caches are stale
    /// until the host service restarts, so we surface this in the
    /// response body too.
    pub restart_recommended: bool,
}

#[utoipa::path(
    post,
    path = "/api/admin/factory-reset",
    request_body = FactoryResetRequest,
    responses(
        (status = 200, description = "All user data wiped", body = FactoryResetResponse),
        (status = 400, description = "Missing or wrong confirm token"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "settings"
)]
pub async fn factory_reset_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<FactoryResetRequest>,
) -> Result<Json<FactoryResetResponse>, ApiError> {
    require_controller(&state, &user)?;
    if req.confirm != CONFIRM_TOKEN {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "confirm_required",
            message: format!("expected confirm = \"{CONFIRM_TOKEN}\""),
        });
    }

    // Step 1 — run the per-plugin purge lifecycle against every
    // installed plugin. This is the load-bearing step that gives an
    // operator a *real* clean slate (containers, on-disk state
    // dirs, OAuth grants, vault secrets, artifact blobs, stage dirs
    // all gone — see plugin_lifecycle module docs for the ordering
    // rationale). Tool-only plugins purge cleanly too; the
    // sidecar-removal step is a no-op for them.
    //
    // We do this FIRST, before any DB nuke, because the per-plugin
    // routines need `state_plugins.stage_path`, the OAuth/vault FKs,
    // and the registry's `RegisteredSidecar.plugin_id` field — all
    // gone after `rebuild_to_empty`. Best-effort by design: errors
    // are captured in each `PluginPurgeReport.errors` and the loop
    // continues.
    let plugins_purged = purge_all_plugins(&state).await;
    tracing::info!(
        target: "factory_reset",
        plugin_count = plugins_purged.len(),
        "plugin purges complete",
    );

    // Step 2 — orphan-directory sweep. Some on-disk state isn't
    // attributed to a specific plugin (research workspaces, stage
    // dirs for plugins with hand-edited state_plugins rows, partial
    // uploads). Recursively `rm -rf` the parent dirs that the
    // per-plugin purges may have left non-empty so the
    // factory-reset response body can promise "nothing on disk
    // survived."
    let orphan_dirs_removed = sweep_orphan_directories();
    tracing::info!(
        target: "factory_reset",
        dirs_removed = orphan_dirs_removed.len(),
        "orphan directory sweep complete",
    );

    // Step 3 — rebuild the DB at the file level. After this returns
    // Ok, the row backing the caller's JWT is gone and the SPA
    // must redirect to /login.
    let (tables_wiped, migrations_reapplied) =
        wipe_and_remigrate(&state.db, &state.db_config).map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "factory_reset_failed",
            message: e.to_string(),
        })?;

    Ok(Json(FactoryResetResponse {
        tables_wiped,
        migrations_reapplied,
        plugins_purged,
        orphan_dirs_removed,
        restart_recommended: true,
    }))
}

/// Recursively remove the top-level `~/.execlaw/` subdirectories that
/// hold plugin / runtime / research state. Per-plugin purges (step 1
/// of factory reset) already remove what they can attribute to a
/// known `plugin_id`; this is the cleanup pass for everything else:
///
///   * `~/.execlaw/sidecars/`  — any sidecar state dir whose plugin
///                               had its `state_plugins` row hand-
///                               edited away (so the purge step
///                               couldn't find it).
///   * `~/.execlaw/plugins/`   — staged plugin ZIPs (future home,
///                               currently unused but reserved).
///   * `~/.execlaw/plugin_artifacts/` — content-addressed artifact
///                               blobs whose `state_artifacts` row
///                               survived a crash or hand-edit.
///   * `~/.execlaw/research/`  — research-job workspaces (one dir
///                               per job; cleaned by job lifecycle
///                               normally but cancelled / orphaned
///                               jobs leave dirs behind).
///
/// Returns the absolute paths actually removed (so the operator can
/// verify what disappeared). A dir that didn't exist returns
/// nothing; a dir that failed to delete is logged at WARN and
/// omitted from the return value.
fn sweep_orphan_directories() -> Vec<String> {
    use directories::UserDirs;
    let home = match UserDirs::new() {
        Some(d) => d.home_dir().to_path_buf(),
        None => return Vec::new(),
    };
    let base = home.join(".execlaw");
    let candidates = ["sidecars", "plugins", "plugin_artifacts", "research"];
    let mut removed = Vec::new();
    for c in candidates {
        let path = base.join(c);
        if !path.exists() {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                tracing::info!(
                    target: "factory_reset",
                    path = %path.display(),
                    "removed orphan directory",
                );
                removed.push(path.display().to_string());
            }
            Err(e) => {
                tracing::warn!(
                    target: "factory_reset",
                    path = %path.display(),
                    error = %e,
                    "failed to remove orphan directory — manual cleanup may be needed",
                );
            }
        }
    }
    removed
}

/// True factory reset: close the SQLite connection, delete the on-
/// disk file (plus `-wal` / `-shm` / `-journal` companions), re-open
/// at the same path with the same encryption posture, then re-run the
/// embedded migration set from scratch. Pulled out for direct
/// unit-test access.
///
/// Rationale (rework 2026-05-13, then 2026-05-14). Two earlier
/// approaches failed in production:
///
/// 1. **`DELETE FROM` every row but keep `schema_version`.** Left
///    migration-seeded singletons (`config_research`,
///    `config_personality`, etc.) empty since the seed `INSERT OR
///    IGNORE` statements only fire as part of the migration body.
///    Downstream code reading them via `query_row(...)?` blew up
///    with `sqlite error: Query returned no rows` — surfaced as
///    "deep research stalls minutes after a fresh-account setup."
///
/// 2. **`DROP TABLE` every table + re-run migrations.** Tripped on
///    SQLite virtual tables (FTS5 `skill_search`, etc.) — `DROP
///    TABLE` on a vtable invokes the module's destructor, and a
///    mismatched module-registration state surfaces as
///    `vtable constructor failed: skill_search`. Observed in
///    production today.
///
/// Both failures share a root cause: trying to be clever about which
/// rows / objects to remove while keeping the file handle open. The
/// robust answer is to **close the connection, delete the file, and
/// re-open empty** — the semantic an operator actually wants from a
/// "factory reset" button. SQLite never opens its own vtable modules
/// against a non-existent file, so the FTS5 destructor never runs
/// against a stale shadow-table state. Schema-level FK ordering and
/// `sqlite_master` enumeration both disappear as concerns.
///
/// Flow:
///
///   1. `Database::rebuild_to_empty(config)` swaps in a temporary
///      in-memory connection (releasing the file handle), deletes
///      the `.db` + `.db-wal` + `.db-shm` + `.db-journal` files,
///      then opens a fresh empty `.db` at the same path with the
///      same encryption posture. `:memory:` DBs short-circuit to a
///      new in-memory connection.
///   2. `MigrationRunner::apply_all()` walks the embedded migration
///      set against the now-empty DB — `CREATE TABLE` every schema,
///      `INSERT OR IGNORE` every singleton seed.
///
/// Returns `(tables_wiped_proxy, migrations_reapplied)`. The
/// `tables_wiped_proxy` value is the count of CREATE TABLE
/// statements the migration set ran, which equals the count of
/// tables in the resulting DB — a useful number for the SPA's
/// post-reset toast even though no individual DROP happened (file
/// delete is one shot).
fn wipe_and_remigrate(
    db: &execlaw_core::Database,
    config: &execlaw_core::DbConfig,
) -> Result<(usize, usize), execlaw_core::DbError> {
    use execlaw_core::migrations::MigrationRunner;

    // Close + delete + re-open. The single load-bearing operation:
    // after this returns Ok, the DB exists at the same path, encrypted
    // with the same key (if any), and contains zero schema.
    db.rebuild_to_empty(config)?;

    // Apply migrations against the now-virgin DB. Re-creates schema
    // and re-fires every `INSERT OR IGNORE` seed in the migration
    // bodies — restores singleton config rows, default personality,
    // search-provider seeds, etc.
    let applied = MigrationRunner::new(db).apply_all().map_err(|e| {
        execlaw_core::DbError::Migration(format!(
            "factory-reset: re-apply migrations failed: {e}"
        ))
    })?;

    // Count the resulting tables for the response body. Cheap one-
    // round-trip read; matches what a `.tables` dump would show.
    let table_count = db.with_conn(|c| {
        let n: i64 = c.query_row(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    })?;

    Ok((table_count, applied.len()))
}

fn require_controller(state: &AppState, user: &AuthedUser) -> Result<(), ApiError> {
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
            code: "controller_only",
            message: "only a Controller can factory-reset the service".into(),
        }),
    }
}

pub fn factory_reset_router() -> Router<AppState> {
    Router::new().route("/api/admin/factory-reset", post(factory_reset_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::{self, Body};
    use axum::http::{Method, Request, header};
    use tower::ServiceExt;

    async fn setup_controller_token(app: &axum::Router) -> String {
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
        v["access_token"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn rejects_wrong_confirm_token() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/factory-reset")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"confirm":"reset"}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rejects_missing_confirm_token() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/factory-reset")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Missing field → axum extractor 400 (UnprocessableEntity in
        // some axum versions); either way it must not be 200.
        assert!(
            resp.status().is_client_error(),
            "expected 4xx, got {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn wipes_users_so_next_login_fails() {
        let state = test_app_state();
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;

        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/factory-reset")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"confirm":"RESET"}).to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            v["tables_wiped"].as_u64().unwrap() > 0,
            "wiped count must be non-zero"
        );
        assert_eq!(v["restart_recommended"], true);

        // Re-login with the same credentials must now fail with
        // not_initialized — the wipe deleted the controller row and
        // the vault hash, so the system is back to first-boot state.
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "username":"ctrl",
                    "admin_password":"hunter2-longer"
                })
                .to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::CONFLICT,
            "post-reset login should not succeed; got {}",
            resp.status(),
        );
    }

    #[tokio::test]
    async fn response_includes_plugin_lifecycle_fields() {
        // Regression for the 2026-05-14 per-plugin lifecycle rework:
        // the response body must surface `plugins_purged` (Vec<...>)
        // and `orphan_dirs_removed` (Vec<String>) so the SPA can show
        // "wiped these plugins: signal (1 container, 12.4 MB),
        // whatsapp (1 container, 4.1 MB) + cleaned 3 orphan dirs"
        // instead of opaque counters.
        //
        // The test harness installs no plugins and wires no sidecar
        // supervisor, so `plugins_purged` is an empty array and
        // `orphan_dirs_removed` is an empty array (no real
        // `~/.execlaw/*` paths exist in CI) — but the fields MUST
        // exist in the JSON response (the SPA can't render them
        // otherwise). This pins the contract.
        let state = test_app_state();
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;

        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/factory-reset")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"confirm":"RESET"}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            v.get("plugins_purged").is_some(),
            "response must include plugins_purged: {v}",
        );
        assert!(
            v["plugins_purged"].is_array(),
            "plugins_purged must be a JSON array, got: {}",
            v["plugins_purged"],
        );
        assert!(
            v.get("orphan_dirs_removed").is_some(),
            "response must include orphan_dirs_removed: {v}",
        );
        assert!(
            v["orphan_dirs_removed"].is_array(),
            "orphan_dirs_removed must be a JSON array, got: {}",
            v["orphan_dirs_removed"],
        );
        // No plugins installed in the harness → empty arrays.
        assert_eq!(v["plugins_purged"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn config_general_is_re_seeded_after_wipe() {
        // The handler reseeds `config_general` so the SPA's
        // `GET /api/admin/settings/general` doesn't 500 between
        // the wipe and the operator re-running setup.
        let state = test_app_state();
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;

        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/factory-reset")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"confirm":"RESET"}).to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Direct DB read — no auth needed, the singleton must exist.
        let count: i64 = state
            .db
            .with_conn(|c| {
                let n: i64 = c
                    .query_row("SELECT COUNT(*) FROM config_general", [], |r| r.get(0))
                    .map_err(|e| execlaw_core::DbError::Config(e.to_string()))?;
                Ok(n)
            })
            .unwrap();
        assert_eq!(count, 1, "config_general singleton must be re-seeded");
    }

    #[tokio::test]
    async fn every_migration_seeded_singleton_is_re_seeded_after_wipe() {
        // Regression for the 2026-05-13 "deep research stalls after a
        // fresh-account setup" bug. The old `wipe_all_user_tables`
        // DELETEd every row but only re-seeded `config_general`,
        // leaving the other migration-seeded singletons empty. Any
        // reader that did `query_row(...)?` on those rows blew up
        // with `sqlite error: Query returned no rows`.
        //
        // The fix swaps DELETE-then-reseed for DROP-then-remigrate,
        // which re-fires every `INSERT OR IGNORE` in the migration
        // bodies. This test pins that behaviour by enumerating the
        // singleton seeds known to be in `0001_baseline.sql` and
        // asserting each ends up with at least one row after the
        // reset.
        let state = test_app_state();
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;

        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/factory-reset")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"confirm":"RESET"}).to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            v["migrations_reapplied"].as_u64().unwrap() > 0,
            "the response should report a non-zero re-applied count",
        );

        // Every migration-seeded singleton must come back populated.
        // The list mirrors the `INSERT OR IGNORE INTO config_*` lines
        // in `0001_baseline.sql`. If a future migration adds another
        // seeded singleton, add it here too.
        let expected_seeded = [
            "config_general",
            "config_personality",
            "config_research",
            "config_search_providers",
            "config_skills",
        ];
        for table in expected_seeded {
            let count: i64 = state
                .db
                .with_conn(|c| {
                    let n: i64 = c
                        .query_row(
                            &format!("SELECT COUNT(*) FROM {table}"),
                            [],
                            |r| r.get(0),
                        )
                        .map_err(|e| execlaw_core::DbError::Config(e.to_string()))?;
                    Ok(n)
                })
                .unwrap();
            assert!(
                count >= 1,
                "{table} singleton must be re-seeded post-reset (got {count} rows)",
            );
        }
    }

    #[tokio::test]
    async fn research_config_get_works_after_wipe() {
        // The original "deep research stalls" bug surfaced as
        // `ResearchConfigStore::get()` throwing
        // `sqlite error: Query returned no rows` when called against
        // a post-factory-reset DB. Pin the integration here so the
        // fix can't regress without this test failing.
        use execlaw_core::research::ResearchConfigStore;

        let state = test_app_state();
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;

        // Wipe.
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/factory-reset")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"confirm":"RESET"}).to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // The store must return Ok here — no `QueryReturnedNoRows`.
        let cfg = ResearchConfigStore::new(&state.db).get();
        assert!(
            cfg.is_ok(),
            "ResearchConfigStore::get() after factory reset must \
             succeed, got: {cfg:?}",
        );
    }

    #[tokio::test]
    async fn unauth_call_is_rejected() {
        let app = build_router(test_app_state());
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/factory-reset")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"confirm":"RESET"}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
            "unauth call must be rejected; got {}",
            resp.status(),
        );
    }
}
