//! Factory reset — wipe every user-data table back to first-boot state.
//!
//! Surfaced as the "Danger zone" at the bottom of Settings → General. The
//! operator types a literal confirmation string ("RESET") into the SPA;
//! the SPA POSTs it here, the handler clears every non-system table in a
//! single transaction, then re-seeds the singleton `config_general` row
//! so the very next request still has its default state without a
//! migration re-run.
//!
//! Teardown ordering (2026-05-13 rework — was DB-wipe-only before):
//!
//!   1. **`fire_on_disable_for_all`** — every loaded script plugin gets a
//!      last chance to run its own cleanup (revoke OAuth refresh tokens,
//!      send a "going offline" notification on its transport, flush
//!      in-memory state to vault). Best-effort; a misbehaving plugin
//!      cannot block the reset.
//!   2. **`SidecarSupervisor::stop_all`** — every running sidecar
//!      container (signal-cli, wuzapi/whatsapp, …) is stopped via
//!      docker. Without this step the wipe leaves orphaned containers
//!      running under their pre-reset names/ports, which then collide
//!      with the next install. This is the bug that prompted the
//!      rework: WhatsApp's wuzapi container survived a factory reset
//!      and refused to start fresh on the next install.
//!   3. **DB wipe** — every non-system SQLite table is truncated in a
//!      single transaction with `defer_foreign_keys = ON`. Re-seeds
//!      `config_general` so the very next API request has its default
//!      singleton row without waiting on a migration re-run.
//!
//! Scope:
//!
//!   * Wipes ONLY persistent SQLite state + sidecar containers + the
//!     plugin lifecycle hook. In-memory caches (refresh tokens, plugin
//!     host registry, runner / backend supervisors, mcp host, voice
//!     sessions) are NOT touched — the operator should restart the
//!     host service after a factory reset for full hygiene. The SPA
//!     shows that recommendation alongside the success state.
//!
//!   * Filesystem artifacts (research workspaces, plugin staging dir,
//!     attachments, sidecar bind-mounts) are NOT removed. A future
//!     iteration can cover those, but a v1 wipe of DB + containers is
//!     enough for a "go back to a clean slate" operator workflow —
//!     the next setup wizard will happily reuse the same paths.
//!
//! The endpoint is Controller-only and idempotent — calling it twice
//! is harmless. The first call destroys the caller's session (the
//! `users` row backing their JWT is gone), so the SPA must sign-out
//! immediately on the 200 response and route to /login, where the
//! AppBoot guard will detect the missing controller and bounce to
//! /setup.

use crate::auth_extract::AuthedUser;
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
    /// Number of tables that were truncated. Useful for the SPA's
    /// post-reset toast and for tests asserting we covered every
    /// table the migration set declares.
    pub tables_wiped: usize,
    /// Number of plugins whose `on_disable` lifecycle hook fired
    /// without erroring. Excludes plugins that don't declare the
    /// hook. Zero is fine — most plugins have nothing to tear down
    /// beyond what the host's `shutdown()` backstop handles.
    #[serde(default)]
    pub plugins_torn_down: usize,
    /// Number of sidecar containers stopped during the reset.
    /// Operators with WhatsApp / Signal sidecars should see this
    /// > 0; tool-only deployments see 0.
    #[serde(default)]
    pub sidecars_stopped: usize,
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

    // Step 1 — fire `on_disable` for every loaded script plugin so
    // each one gets a last chance to revoke OAuth tokens, send
    // farewell notifications on its transport, flush state, etc.
    // BEFORE the rug-pull. Best-effort: a panicking hook is logged
    // but doesn't block the reset.
    let plugins_torn_down = state.plugin_host.fire_on_disable_for_all().await;
    tracing::info!(
        target: "factory_reset",
        plugins_torn_down,
        "fired on_disable for loaded script plugins",
    );

    // Step 2 — stop every running sidecar container. Without this
    // the wipe leaves orphans (the WhatsApp wuzapi container survived
    // factory reset and refused to start fresh on the next install —
    // that's the bug this rework is fixing). `stop_all` returns the
    // count actually stopped; missing supervisor (tests, no-docker
    // dev builds) is OK — just skip the step.
    let sidecars_stopped = match &state.sidecar_supervisor {
        Some(sup) => sup.stop_all().await,
        None => 0,
    };
    tracing::info!(
        target: "factory_reset",
        sidecars_stopped,
        "stopped sidecar containers",
    );

    // Step 3 — wipe the DB. Done last so the on_disable hooks above
    // can still read vault rows / OAuth tokens / personality config
    // while running their cleanup. Once this returns the row backing
    // the caller's JWT is gone and the SPA must redirect to /login.
    let wiped = wipe_all_user_tables(&state.db).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "factory_reset_failed",
        message: e.to_string(),
    })?;

    Ok(Json(FactoryResetResponse {
        tables_wiped: wiped,
        plugins_torn_down,
        sidecars_stopped,
        restart_recommended: true,
    }))
}

/// Run the actual wipe. Pulled out for direct unit-test access.
///
/// Implementation notes:
///
///   * We discover tables from `sqlite_master` rather than hard-coding
///     a list — that way new migrations are automatically covered.
///   * `schema_version` is preserved so SQLite doesn't think every
///     migration is unapplied on next boot (which would re-run the
///     ones that side-effect on first apply).
///   * `defer_foreign_keys = ON` lets us delete in arbitrary order
///     inside the transaction without tripping the FK constraints
///     that `apply_init_pragmas` enabled at connection-open time.
///   * We re-seed the `config_general` singleton row so the very
///     next `GET /api/admin/settings/general` succeeds without
///     requiring a service restart.
fn wipe_all_user_tables(db: &execlaw_core::Database) -> Result<usize, execlaw_core::DbError> {
    db.transaction(|tx| {
        tx.execute_batch("PRAGMA defer_foreign_keys = ON;")?;
        let mut stmt = tx.prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' \
               AND name NOT LIKE 'sqlite_%' \
               AND name != 'schema_version'",
        )?;
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        drop(stmt);

        for table in &names {
            // SQLite identifiers are wrapped in double-quotes for
            // safety even though our tables don't use anything
            // exotic; double-up any embedded quote per the spec.
            let quoted = table.replace('"', "\"\"");
            tx.execute(&format!("DELETE FROM \"{quoted}\""), [])?;
        }

        // Re-seed the config_general singleton — `OR IGNORE` is a
        // no-op if a future migration adds a different default-row
        // pattern that already inserted between the DELETE and here.
        tx.execute(
            "INSERT OR IGNORE INTO config_general \
                (id, start_on_boot, bind_address, updated_at) \
             VALUES (1, 1, '127.0.0.1:3031', unixepoch())",
            [],
        )?;
        Ok(names.len())
    })
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
    async fn response_includes_teardown_counts_in_body() {
        // Regression for the 2026-05-13 teardown rework: the
        // response body must surface `plugins_torn_down` +
        // `sidecars_stopped` so the SPA can show a meaningful
        // "wiped X tables, stopped Y containers, fired Z plugin
        // teardowns" toast instead of just "tables_wiped".
        //
        // In the test harness no plugins are installed and no
        // sidecar supervisor is wired, so the counts are 0/0 —
        // but the fields MUST exist in the JSON response (the
        // SPA can't render them otherwise). This pins the contract.
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
            v.get("plugins_torn_down").is_some(),
            "response must include plugins_torn_down: {v}",
        );
        assert!(
            v.get("sidecars_stopped").is_some(),
            "response must include sidecars_stopped: {v}",
        );
        // No supervisor + no plugins in the test harness → both 0.
        assert_eq!(v["plugins_torn_down"], 0);
        assert_eq!(v["sidecars_stopped"], 0);
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
