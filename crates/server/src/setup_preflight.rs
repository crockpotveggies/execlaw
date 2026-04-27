//! Phase 14 — first-run setup preflight (`/api/admin/setup/preflight`).
//!
//! After the operator creates their controller account at
//! `POST /api/setup`, the SPA's wizard runs through two more screens
//! (Docker availability + GPU detection / backend setup). Both
//! screens depend on the same backend probe so a single endpoint
//! returns everything they need:
//!
//! ```json
//! {
//!   "docker": { "available": true, "version": "24.0.7" },
//!   "gpus":   [ { "vendor": "Nvidia", ... }, ... ]
//! }
//! ```
//!
//! Docker availability is determined by shelling out to `docker info`
//! — that connects to the daemon, so success means dockerd is up and
//! reachable, not just that the CLI is installed. The version comes
//! from `--format '{{.ServerVersion}}'`. Missing → `available: false,
//! version: null` and the SPA shows the Docker-Desktop install
//! prompt.
//!
//! GPUs come from the same `hardware-query`-backed `detect()` the
//! Backend wizard uses (Phase 14 follow-up).

use crate::auth_extract::AuthedUser;
use crate::state::AppState;
use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use axum::Router;
use execlaw_container_manager::{detect, GpuDevice};
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct PreflightResponse {
    pub docker: DockerStatus,
    /// `Vec<GpuDevice>` — `GpuDevice` is in the container-manager
    /// crate which doesn't depend on utoipa, so we expose it as
    /// opaque JSON in the OpenAPI spec. SPA-side types live in
    /// `web/src/api/endpoints.ts` and stay in sync by hand.
    #[schema(value_type = serde_json::Value)]
    pub gpus: Vec<GpuDevice>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DockerStatus {
    /// `true` when `docker info` returned exit 0 — the CLI is on
    /// PATH AND the daemon answered. SPA gates the "managed
    /// backend" path on this.
    pub available: bool,
    /// Server version reported by `docker info --format
    /// '{{.ServerVersion}}'`. Surface in the SPA when present so
    /// the operator sees what the wizard talks to.
    pub version: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/admin/setup/preflight",
    responses(
        (
            status = 200,
            description = "Docker availability + detected GPUs for the setup wizard",
            body = PreflightResponse,
        ),
    ),
    security(("bearer_jwt" = [])),
    tag = "setup"
)]
pub async fn get_handler(
    State(_state): State<AppState>,
    _user: AuthedUser,
) -> Json<PreflightResponse> {
    let docker = detect_docker();
    let profile = detect();
    Json(PreflightResponse {
        docker,
        gpus: profile.gpus,
    })
}

/// Probe `docker info` to determine Docker daemon availability.
///
/// We use `docker info --format` instead of `docker --version`
/// because the latter only checks the CLI binary; `info` actually
/// connects to the daemon. A daemon-down state (Docker Desktop
/// quit, dockerd not started) reads as `available: false` — which
/// is the right answer for the wizard's "can we manage backends?"
/// question.
fn detect_docker() -> DockerStatus {
    use std::process::Command;
    let output = Command::new("docker")
        .arg("info")
        .arg("--format")
        .arg("{{.ServerVersion}}")
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let version = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            DockerStatus {
                available: true,
                version: if version.is_empty() {
                    None
                } else {
                    Some(version)
                },
            }
        }
        _ => DockerStatus {
            available: false,
            version: None,
        },
    }
}

pub fn setup_preflight_router() -> Router<AppState> {
    Router::new().route("/api/admin/setup/preflight", get(get_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::{self, Body};
    use axum::http::{header, Method, Request, StatusCode};
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
    async fn preflight_returns_docker_and_gpus_shape() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/setup/preflight")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Docker probe is a real shell-out — we don't assert what
        // the host has; just that the schema is right and either
        // shape (available true or false) is present.
        assert!(v["docker"].is_object());
        assert!(v["docker"]["available"].is_boolean());
        assert!(v["docker"]["version"].is_string() || v["docker"]["version"].is_null());
        assert!(v["gpus"].is_array());
    }

    #[tokio::test]
    async fn preflight_requires_auth() {
        let app = build_router(test_app_state());
        // No token at all.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/setup/preflight")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            resp.status() == StatusCode::UNAUTHORIZED
                || resp.status() == StatusCode::FORBIDDEN,
            "expected 401/403, got {}",
            resp.status()
        );
    }

    #[test]
    fn detect_docker_returns_well_formed_status() {
        // Smoke: detect_docker doesn't panic and returns a coherent
        // shape regardless of whether docker is installed on the
        // test runner.
        let s = detect_docker();
        if s.available {
            // When available, version SHOULD be present (docker
            // info --format ServerVersion always returns something).
            // We tolerate empty version on edge-case dockerd builds.
            let _ = s.version;
        } else {
            assert!(s.version.is_none());
        }
    }
}
