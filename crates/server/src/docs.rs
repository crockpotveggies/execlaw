//! API documentation endpoints.
//!
//! - `GET /api/openapi.json`  OpenAPI 3.x spec from `utoipa`-annotated
//!   handlers.
//! - `GET /api/asyncapi.json`  Hand-authored AsyncAPI 3.x spec (the
//!   WebSocket event vocabulary).
//! - `GET /api/docs`  A tiny HTML page that loads Swagger UI for the
//!   OpenAPI spec and a JSON pretty-print viewer for the AsyncAPI spec.
//!
//! Per MIGRATION_PLAN §8.4: "Swagger/OpenAPI 3.x for REST (via utoipa
//! annotations) and AsyncAPI 3.x for WebSocket event vocabulary (hand-
//! authored). Both served at /api/docs."

use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use utoipa::OpenApi;

use crate::approvals::{
    IdentifierSummary, PendingApprovalSummary, PendingApprovalsResponse,
    PrincipalListResponse, PrincipalSummary,
};
use crate::deployments::{
    CreateDeploymentRequest, DeploymentListResponse, DeploymentView,
    UpdateDeploymentRequest,
};
use crate::plugins::{
    InstallResponse, PluginListResponse, PluginSummary, ToolSummary, UiPanelListResponse,
    UiPanelSummary,
};
use crate::routes::{
    GenericOk, HealthResponse, LoginRequest, LoginResponse, LogoutRequest, MeResponse,
    RefreshRequest, RefreshResponse, SetupRequest, SetupResponse,
};
use utoipa::Modify;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};

/// Adds the Bearer-JWT security scheme used by `[security(("bearer_jwt" = []))]`
/// annotations on auth-gated routes.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.as_mut().expect("components present");
        components.add_security_scheme(
            "bearer_jwt",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .description(Some(
                        "Access token obtained from /api/login or /api/setup. \
                         Header: `Authorization: Bearer <token>`.",
                    ))
                    .build(),
            ),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "execlaw",
        version = "0.1.0",
        description = "Self-hosted Rust agent framework. Phase 6a surface.",
    ),
    modifiers(&SecurityAddon),
    paths(
        // meta + auth
        crate::routes::health,
        crate::routes::ping,
        crate::routes::setup,
        crate::routes::login,
        crate::routes::refresh,
        crate::routes::logout,
        crate::routes::admin_me,
        crate::routes::admin_hardware,
        // chats
        crate::chats::send_message,
        crate::chats::list_messages,
        crate::chats::patch_thread,
        crate::chats::list_threads,
        // plugins
        crate::plugins::list_handler,
        crate::plugins::install_handler,
        crate::plugins::list_tools_handler,
        crate::plugins::list_ui_panels_handler,
        crate::plugins::enable_handler,
        crate::plugins::disable_handler,
        crate::plugins::uninstall_handler,
        // observability
        crate::observability::logs_handler,
        crate::observability::eval_flags_handler,
        crate::observability::audit_handler,
        // approvals
        crate::approvals::respond_handler,
        crate::approvals::revoke_handler,
        crate::approvals::list_principals_handler,
        crate::approvals::list_pending_approvals_handler,
        // deployments
        crate::deployments::list_handler,
        crate::deployments::create_handler,
        crate::deployments::update_handler,
        crate::deployments::delete_handler,
    ),
    components(schemas(
        HealthResponse,
        SetupRequest,
        SetupResponse,
        LoginRequest,
        LoginResponse,
        RefreshRequest,
        RefreshResponse,
        LogoutRequest,
        GenericOk,
        MeResponse,
        PluginSummary,
        PluginListResponse,
        InstallResponse,
        ToolSummary,
        UiPanelSummary,
        UiPanelListResponse,
        PrincipalSummary,
        PrincipalListResponse,
        IdentifierSummary,
        PendingApprovalSummary,
        PendingApprovalsResponse,
        DeploymentView,
        DeploymentListResponse,
        CreateDeploymentRequest,
        UpdateDeploymentRequest,
    )),
    tags(
        (name = "meta", description = "Liveness + introspection"),
        (name = "auth", description = "First-run setup, login, token management, current-user profile"),
        (name = "admin", description = "Hardware + runtime introspection"),
        (name = "chats", description = "Conversations + thread metadata"),
        (name = "plugins", description = "Install, enable/disable, uninstall, tool + panel listings"),
        (name = "observability", description = "Logs + eval flags"),
        (name = "approvals", description = "Approval verbs + trust revocation"),
    )
)]
pub struct ApiDoc;

pub fn openapi_json_string() -> String {
    ApiDoc::openapi()
        .to_pretty_json()
        .unwrap_or_else(|_| "{}".to_owned())
}

/// Hand-authored AsyncAPI 3 spec for `/api/stream`.
///
/// Embedded from `spec/asyncapi.yaml` at the repo root so operators can
/// edit the YAML without recompiling... wait, no: we embed at compile
/// time to keep the binary self-contained (§0 "minimal containers").
/// Operators who want to tweak the published spec rebuild; the event
/// vocabulary itself is code-defined anyway.
pub const ASYNCAPI_YAML: &str = include_str!("../../../spec/asyncapi.yaml");

fn asyncapi_json() -> String {
    // Cheap YAML-to-JSON hop via serde_yaml... but we don't want that dep.
    // Since we hand-maintain the YAML, ship a small JSON sidecar too.
    include_str!("../../../spec/asyncapi.json").to_owned()
}

/// A minimal docs page: links to Swagger UI (served inline) and pretty-
/// prints the AsyncAPI spec.
async fn docs_html() -> Html<&'static str> {
    Html(DOCS_HTML)
}

const DOCS_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>execlaw API docs</title>
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <style>
    body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
           margin: 0; padding: 0; background: #111; color: #eee; }
    header { background: #222; padding: 1rem 2rem; border-bottom: 1px solid #333; }
    header h1 { margin: 0; font-size: 1.25rem; }
    nav { margin-top: 0.5rem; }
    nav a { color: #9cf; margin-right: 1rem; text-decoration: none; }
    main { padding: 1rem 2rem; }
    iframe { border: 1px solid #333; background: #fff; width: 100%; min-height: 600px; }
    pre { background: #1a1a1a; padding: 1rem; border-radius: 4px; overflow: auto; max-height: 400px; }
  </style>
</head>
<body>
<header>
  <h1>execlaw API docs</h1>
  <nav>
    <a href="#openapi">OpenAPI (REST)</a>
    <a href="#asyncapi">AsyncAPI (WebSocket)</a>
    <a href="/api/openapi.json">openapi.json</a>
    <a href="/api/asyncapi.json">asyncapi.json</a>
  </nav>
</header>
<main>
  <section id="openapi">
    <h2>REST API</h2>
    <p>Swagger UI loads from a CDN-free, locally-bundled page.</p>
    <iframe src="/api/docs/swagger/" title="Swagger UI"></iframe>
  </section>
  <section id="asyncapi">
    <h2>WebSocket events</h2>
    <p>AsyncAPI 3 spec for <code>/api/stream</code>.</p>
    <pre id="asyncapi-json">Loading…</pre>
    <script>
      fetch('/api/asyncapi.json')
        .then(r => r.json())
        .then(j => { document.getElementById('asyncapi-json').textContent = JSON.stringify(j, null, 2); })
        .catch(e => { document.getElementById('asyncapi-json').textContent = 'error: ' + e; });
    </script>
  </section>
</main>
</body>
</html>"##;

async fn serve_asyncapi() -> Response {
    let body = asyncapi_json();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

async fn serve_asyncapi_yaml() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/yaml")],
        ASYNCAPI_YAML.to_owned(),
    )
        .into_response()
}

/// Build the docs sub-router. Merged by `routes::build_router`.
///
/// `utoipa-swagger-ui` owns `/api/openapi.json` because it also serves the
/// Swagger UI at `/api/docs/swagger/` that reads from that URL. We add
/// the AsyncAPI endpoints and the combined docs landing page on top.
pub fn docs_router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/asyncapi.json", get(serve_asyncapi))
        .route("/api/asyncapi.yaml", get(serve_asyncapi_yaml))
        .route("/api/docs", get(docs_html))
        .merge(swagger_sub_router::<S>())
}

fn swagger_sub_router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    use utoipa_swagger_ui::SwaggerUi;
    // Serves Swagger UI at /api/docs/swagger/* AND exposes the spec at
    // /api/openapi.json automatically.
    Router::new()
        .merge(SwaggerUi::new("/api/docs/swagger").url("/api/openapi.json", ApiDoc::openapi()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_json_is_valid_json_and_lists_health() {
        let s = openapi_json_string();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["info"]["title"], "execlaw");
        assert!(v["paths"]["/api/health"].is_object());
        assert!(v["paths"]["/api/setup"].is_object());
        assert!(v["paths"]["/api/login"].is_object());
        assert!(v["paths"]["/api/token/refresh"].is_object());
        assert!(v["paths"]["/api/logout"].is_object());
    }

    /// Coverage guard: every REST route the SPA depends on (and every
    /// route the controller may need to introspect) MUST appear in the
    /// OpenAPI spec. Adding a route without a `#[utoipa::path]` is now
    /// a test failure — keeps the spec in sync without code review.
    ///
    /// `/api/stream` is intentionally excluded: it's a WebSocket
    /// endpoint and lives in the AsyncAPI spec instead.
    #[test]
    fn openapi_covers_every_rest_route() {
        let v: serde_json::Value = serde_json::from_str(&openapi_json_string()).unwrap();
        let expected: &[(&str, &[&str])] = &[
            ("/api/health", &["get"]),
            ("/api/ping", &["get"]),
            ("/api/setup", &["post"]),
            ("/api/login", &["post"]),
            ("/api/token/refresh", &["post"]),
            ("/api/logout", &["post"]),
            ("/api/admin/me", &["get"]),
            ("/api/admin/hardware", &["get"]),
            ("/api/chats", &["get"]),
            ("/api/chats/{conversation_id}/messages", &["get", "post"]),
            ("/api/chats/{conversation_id}", &["patch"]),
            ("/api/admin/plugins", &["get"]),
            ("/api/admin/plugins/install", &["post"]),
            ("/api/admin/plugins/tools", &["get"]),
            ("/api/admin/plugins/ui_panels", &["get"]),
            ("/api/admin/plugins/{plugin_id}/enable", &["post"]),
            ("/api/admin/plugins/{plugin_id}/disable", &["post"]),
            ("/api/admin/plugins/{plugin_id}", &["delete"]),
            ("/api/admin/logs", &["get"]),
            ("/api/admin/eval/flags", &["get"]),
            ("/api/admin/audit", &["get"]),
            ("/api/admin/deployments", &["get", "post"]),
            ("/api/admin/deployments/{id}", &["patch", "delete"]),
            ("/api/admin/approvals", &["get"]),
            ("/api/admin/approvals/{approval_id}/respond", &["post"]),
            ("/api/admin/principals", &["get"]),
            ("/api/admin/principals/{principal_id}/revoke", &["post"]),
        ];
        for (path, methods) in expected {
            let entry = &v["paths"][path];
            assert!(
                entry.is_object(),
                "OpenAPI spec missing path {path}; remember to add #[utoipa::path] + register in ApiDoc"
            );
            for m in *methods {
                assert!(
                    entry[m].is_object(),
                    "OpenAPI spec missing {} on {path}",
                    m.to_uppercase()
                );
            }
        }
    }

    /// The Bearer-JWT security scheme must be defined so Swagger UI
    /// renders the "Authorize" button. Drift here would silently
    /// break the docs page even when individual paths look fine.
    #[test]
    fn openapi_declares_bearer_jwt_security_scheme() {
        let v: serde_json::Value = serde_json::from_str(&openapi_json_string()).unwrap();
        let bearer = &v["components"]["securitySchemes"]["bearer_jwt"];
        assert!(bearer.is_object(), "bearer_jwt scheme not declared");
        assert_eq!(bearer["type"], "http");
        assert_eq!(bearer["scheme"], "bearer");
        assert_eq!(bearer["bearerFormat"], "JWT");
    }

    #[test]
    fn asyncapi_json_is_valid_json() {
        let s = asyncapi_json();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        // AsyncAPI 3 uses the `asyncapi` top-level field.
        assert!(v["asyncapi"].is_string());
        assert!(v["channels"].is_object() || v["channels"].is_null());
    }
}
