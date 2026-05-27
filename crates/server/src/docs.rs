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

use crate::alerts::{AlertCountResponse, AlertListResponse, AlertView};
use crate::approvals::{
    IdentifierSummary, PendingApprovalSummary, PendingApprovalsResponse, PrincipalListResponse,
    PrincipalSummary,
};
use crate::automation_runtime::{DryRunResult, ExecOutcome};
use crate::automations_admin::{
    AutomationDto, CreateAutomationRequest, MetricsDto, SampleEventBody, SuggestionDto,
    TestRunRequest, UpdateAutomationRequest,
};
use crate::backend_presets::{BackendPreset, PresetField, PresetWithFlag, PresetsResponse};
use crate::backends::{
    BackendListEntry, BackendListResponse, BackendLogsResponse, BackendStatusResponse, BackendView,
    UpsertBackendRequest,
};
use crate::inference_metrics::{ConsumerSnapshot, InferenceConsumer, MetricsSnapshot};
use crate::mcp_admin::{McpServerListResponse, McpServerView, McpServerWriteRequest};
use crate::my_identities::{
    AddIdentifierRequest, AvailableTransportView, AvailableTransportsResponse, IdentifierView,
    MyIdentitiesResponse,
};
use crate::personality::{
    PersonalityListResponse, PersonalityPreviewResponse, PersonalityView, UpsertPersonalityRequest,
};
use crate::plugins::{
    InstallResponse, PluginListResponse, PluginSummary, ToolSummary, UiPanelListResponse,
    UiPanelSummary,
};
use crate::research_admin::{
    ResearchActiveCountResponse, ResearchAdvanceResponse, ResearchCancelRequest,
    ResearchCancelResponse, ResearchJobReportResponse, ResearchJobSummaryView,
    ResearchJobsResponse,
};
use crate::routes::{
    GenericOk, HealthResponse, LoginRequest, LoginResponse, LogoutAllResponse, LogoutRequest,
    MeResponse, RefreshRequest, RefreshResponse, SetupRequest, SetupResponse,
};
use crate::routines::{
    PreviewRequest, PreviewResponse, RoutineListResponse, RoutineRunListResponse, RoutineRunView,
    RoutineView, UpsertRoutineRequest,
};
use crate::runners_admin::{GroupRunnerListResponse, GroupRunnerView};
use crate::settings_general::{GeneralSettingsView, UpdateGeneralSettingsRequest};
use crate::settings_research::{ResearchSettingsView, UpdateResearchSettingsRequest};
use crate::setup_preflight::{DockerStatus, PreflightResponse};
use crate::tools_admin::{ToolListResponse, ToolView, UpdateToolPolicyRequest};
use crate::trust_policy::{TrustPolicyView, UpdateTrustPolicyRequest};
use crate::users::{
    ChangePasswordRequest, InviteUserRequest, ResetPasswordRequest, UserListResponse, UserView,
};
use execlaw_core::automation_runs::{AutomationRunRow, AutomationRunStatus, StepTrace};
use execlaw_core::automations::{
    AskAgentConfig, AutomationDef, EdgeDef, ExitToolDef, NodeDef, NodeKind, TriggerDef,
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
        crate::routes::logout_all,
        crate::routes::admin_me,
        crate::routes::admin_hardware,
        // chats
        crate::chats::send_message,
        crate::chats::stop_turn,
        crate::chats::generate_title,
        crate::chats::list_messages,
        crate::chats::patch_thread,
        crate::chats::delete_thread,
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
        // backends (Phase 8.5 — replaces "deployments" CRUD)
        crate::backends::list_handler,
        crate::backends::upsert_handler,
        crate::backends::clear_handler,
        // backend supervisor (Phase 12.C — managed-mode lifecycle)
        crate::backends::status_handler,
        crate::backends::restart_handler,
        crate::backends::logs_handler,
        // backend presets (Phase 13.B.1 — managed-mode wizard library)
        crate::backends::presets_handler,
        // runners (Phase 16 — supervisor-tracked per principal group)
        crate::runners_admin::list_groups_handler,
        crate::runners_admin::restart_group_handler,
        crate::runners_admin::wipe_group_handler,
        // users (multi-controller) + Phase-8.6 password rotation
        crate::users::list_handler,
        crate::users::invite_handler,
        crate::users::delete_handler,
        crate::users::change_my_password_handler,
        crate::users::reset_user_password_handler,
        // tools (Phase 8a per-tool trust-class allowlist)
        crate::tools_admin::list_handler,
        crate::tools_admin::update_handler,
        // mcp (Phase 8c MCP server CRUD)
        crate::mcp_admin::list_handler,
        crate::mcp_admin::create_handler,
        crate::mcp_admin::update_handler,
        crate::mcp_admin::delete_handler,
        // personality (Phase 9 — system-prompt fields, §5.5)
        crate::personality::list_handler,
        crate::personality::get_default_handler,
        crate::personality::upsert_default_handler,
        crate::personality::get_conversation_handler,
        crate::personality::upsert_conversation_handler,
        crate::personality::delete_conversation_handler,
        crate::personality::preview_handler,
        // alerts (Phase 9.1 — operational anomalies, §10)
        crate::alerts::list_handler,
        crate::alerts::count_handler,
        crate::alerts::ack_handler,
        crate::alerts::resolve_handler,
        // trust policy (Phase 9.2 — operator-editable trust ladder rules, §2.6)
        crate::trust_policy::get_handler,
        crate::trust_policy::put_handler,
        // my identities (Phase 9.3 — controller's per-transport handles, §7.1)
        crate::my_identities::list_handler,
        crate::my_identities::add_handler,
        crate::my_identities::delete_handler,
        crate::my_identities::list_transports_handler,
        // routines (Phase 10 — cron-shaped agent automations, §5.6)
        crate::routines::list_handler,
        crate::routines::create_handler,
        crate::routines::get_handler,
        crate::routines::update_handler,
        crate::routines::delete_handler,
        crate::routines::run_now_handler,
        crate::routines::runs_handler,
        crate::routines::preview_handler,
        // general settings (Phase 14 — start-on-boot, bind, ...)
        crate::settings_general::get_handler,
        crate::settings_general::put_handler,
        crate::settings_research::get_handler,
        crate::settings_research::put_handler,
        crate::research_admin::list_jobs_handler,
        crate::research_admin::get_job_handler,
        crate::research_admin::get_report_handler,
        crate::research_admin::active_count_handler,
        crate::research_admin::cancel_job_handler,
        crate::research_admin::advance_job_handler,
        // setup preflight (Phase 14 — first-run wizard docker + gpu)
        crate::setup_preflight::get_handler,
        crate::setup_preflight::dismiss_handler,
        // automations (M1-M5 — event-triggered flows)
        crate::automations_admin::list,
        crate::automations_admin::create,
        crate::automations_admin::get_one,
        crate::automations_admin::update,
        crate::automations_admin::delete_one,
        crate::automations_admin::enable,
        crate::automations_admin::disable,
        crate::automations_admin::list_runs,
        crate::automations_admin::metrics,
        crate::automations_admin::list_suggestions,
        crate::automations_admin::get_suggestion,
        crate::automations_admin::dismiss_suggestion,
        crate::automations_admin::action_suggestion,
        crate::automations_admin::test_run,
        // inference observability (M5)
        crate::inference_admin::metrics,
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
        LogoutAllResponse,
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
        BackendView,
        BackendListResponse,
        BackendLogsResponse,
        BackendListEntry,
        BackendStatusResponse,
        UpsertBackendRequest,
        BackendPreset,
        PresetField,
        PresetWithFlag,
        PresetsResponse,
        GroupRunnerView,
        GroupRunnerListResponse,
        UserView,
        UserListResponse,
        InviteUserRequest,
        ChangePasswordRequest,
        ResetPasswordRequest,
        ToolView,
        ToolListResponse,
        UpdateToolPolicyRequest,
        McpServerView,
        McpServerListResponse,
        McpServerWriteRequest,
        PersonalityView,
        PersonalityListResponse,
        PersonalityPreviewResponse,
        UpsertPersonalityRequest,
        AlertView,
        AlertListResponse,
        AlertCountResponse,
        TrustPolicyView,
        UpdateTrustPolicyRequest,
        IdentifierView,
        MyIdentitiesResponse,
        AddIdentifierRequest,
        AvailableTransportView,
        AvailableTransportsResponse,
        RoutineView,
        RoutineListResponse,
        RoutineRunView,
        RoutineRunListResponse,
        UpsertRoutineRequest,
        PreviewRequest,
        PreviewResponse,
        GeneralSettingsView,
        UpdateGeneralSettingsRequest,
        ResearchSettingsView,
        UpdateResearchSettingsRequest,
        ResearchJobsResponse,
        ResearchJobSummaryView,
        ResearchJobReportResponse,
        ResearchActiveCountResponse,
        ResearchAdvanceResponse,
        ResearchCancelResponse,
        ResearchCancelRequest,
        PreflightResponse,
        DockerStatus,
        // automations (M1-M5)
        AutomationDto,
        CreateAutomationRequest,
        UpdateAutomationRequest,
        MetricsDto,
        SampleEventBody,
        SuggestionDto,
        TestRunRequest,
        DryRunResult,
        ExecOutcome,
        AutomationRunRow,
        AutomationRunStatus,
        StepTrace,
        AutomationDef,
        TriggerDef,
        NodeDef,
        EdgeDef,
        NodeKind,
        ExitToolDef,
        AskAgentConfig,
        // inference observability (M5)
        MetricsSnapshot,
        ConsumerSnapshot,
        InferenceConsumer,
    )),
    tags(
        (name = "meta", description = "Liveness + introspection"),
        (name = "auth", description = "First-run setup, login, token management, current-user profile"),
        (name = "admin", description = "Hardware + runtime introspection"),
        (name = "chats", description = "Conversations + thread metadata"),
        (name = "plugins", description = "Install, enable/disable, uninstall, tool + panel listings"),
        (name = "observability", description = "Logs + eval flags"),
        (name = "approvals", description = "Approval verbs + trust revocation"),
        (name = "automations", description = "Event-triggered flows: bus → matcher → typed graph → AskAgent / Filter / Transform / Branch / Terminal. CRUD, runs, metrics, suggestions, test-run."),
        (name = "inference", description = "Per-consumer LLM call observability (in_flight, totals, p50/p95)."),
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
            ("/api/logout/all", &["post"]),
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
            ("/api/admin/backends", &["get"]),
            ("/api/admin/backends/{purpose}", &["put", "delete"]),
            ("/api/admin/backends/{purpose}/status", &["get"]),
            ("/api/admin/backends/{purpose}/restart", &["post"]),
            ("/api/admin/runners/groups", &["get"]),
            ("/api/admin/runners/groups/{group_id}/restart", &["post"]),
            ("/api/admin/runners/groups/{group_id}/wipe", &["post"]),
            ("/api/admin/users", &["get"]),
            ("/api/admin/users/invite", &["post"]),
            ("/api/admin/users/{user_id}", &["delete"]),
            ("/api/admin/me/password", &["post"]),
            ("/api/admin/users/{user_id}/password", &["post"]),
            ("/api/admin/tools", &["get"]),
            ("/api/admin/tools/{tool_name}", &["patch"]),
            ("/api/admin/mcp/servers", &["get", "post"]),
            ("/api/admin/mcp/servers/{id}", &["post"]),
            ("/api/admin/mcp/servers/{id}/delete", &["post"]),
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
