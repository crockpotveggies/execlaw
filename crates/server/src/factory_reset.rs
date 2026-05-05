//! Factory reset — wipe every user-data table back to first-boot state.
//!
//! Surfaced as the "Danger zone" at the bottom of Settings → General. The
//! operator types a literal confirmation string ("RESET") into the SPA;
//! the SPA POSTs it here, the handler clears every non-system table in a
//! single transaction, then re-seeds the singleton `config_general` row
//! so the very next request still has its default state without a
//! migration re-run.
//!
//! Scope:
//!
//!   * Wipes ONLY persistent SQLite state. In-memory caches (refresh
//!     tokens, plugin host registry, runner / backend supervisors,
//!     mcp host, voice sessions) are NOT touched — the operator should
//!     restart the host service after a factory reset for full
//!     hygiene. The SPA shows that recommendation alongside the
//!     success state.
//!
//!   * Filesystem artifacts (research workspaces, plugin staging dir,
//!     attachments) are NOT removed. A future iteration can cover
//!     those, but a v1 wipe of the DB is enough for a "go back to a
//!     clean slate" operator workflow — the next setup wizard will
//!     happily reuse the same paths.
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

    let wiped = wipe_all_user_tables(&state.db).map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "factory_reset_failed",
        message: e.to_string(),
    })?;

    Ok(Json(FactoryResetResponse {
        tables_wiped: wiped,
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
             VALUES (1, 1, '127.0.0.1:3030', unixepoch())",
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
