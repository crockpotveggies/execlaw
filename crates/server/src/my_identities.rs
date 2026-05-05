//! Settings → My Identities (Phase 9.3, MIGRATION_PLAN §7.1).
//!
//! v1 surface: list, add, and remove transport-scoped identifiers
//! attached to the **controller's** principal. The controller is
//! already trusted system-wide, so v1 takes the operator's assertion
//! at face value — verification challenges (email magic-link, SMS
//! code, Signal device-link) land later when the corresponding
//! transport plugins ship.
//!
//! When the controller declares e.g. `signal:+15551234`, an inbound
//! Signal message from that handle resolves through
//! `PrincipalStore::find_by_identifier` → the controller principal,
//! and the cold-contact ladder skips entirely. That's the whole point
//! of this surface: keeping outbound operator presence on multiple
//! channels coherent with inbound trust resolution.
//!
//! Routes:
//!   * `GET    /api/admin/me/identifiers` — list current identifiers.
//!     Returns `{ identifiers: [], controller_principal_id }` even
//!     when no principal row exists yet.
//!   * `POST   /api/admin/me/identifiers` — add `{transport, handle}`.
//!     Lazily creates the principal row on first add.
//!   * `DELETE /api/admin/me/identifiers/{transport}/{handle}` — drop
//!     a single identifier.

use crate::auth_extract::AuthedUser;
use crate::routes::{ApiError, controller_principal_id};
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use execlaw_core::PrincipalId;
use execlaw_core::audit::AuditStore;
use execlaw_core::principal::{Identifier, Principal, PrincipalStore, TrustLevel};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct IdentifierView {
    pub transport: String,
    pub handle: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MyIdentitiesResponse {
    pub controller_principal_id: String,
    pub identifiers: Vec<IdentifierView>,
}

/// One transport the SPA can populate the "My identities" dropdown
/// with. Built-in entries (`web`, `voice`) come from the platform;
/// plugin entries reflect installed `[transport]` declarations
/// (Signal, future WhatsApp / Matrix / etc.). The SPA renders
/// `label` to the operator and submits `id` back via the add form.
#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct AvailableTransportView {
    /// Channel id — same string the inbound consumer sets on
    /// `state_transport_bindings.channel`. The SPA submits this
    /// verbatim back through the add form.
    pub id: String,
    /// Operator-facing label. Built-ins hardcode a friendly
    /// rendering; plugin transports use the plugin's manifest
    /// `[plugin].name`.
    pub label: String,
    /// Source attribution so the SPA can chip "(plugin)" /
    /// "(built-in)" in the dropdown — useful when an operator is
    /// debugging "why is Signal not showing up here." `None` for
    /// built-in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// Suggested handle placeholder — e.g. `+15551234` for Signal,
    /// `you@example.com` for email. Empty string when the bridge
    /// doesn't have a stable handle shape.
    pub handle_placeholder: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AvailableTransportsResponse {
    pub transports: Vec<AvailableTransportView>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AddIdentifierRequest {
    pub transport: String,
    pub handle: String,
}

/// `GET /api/admin/me/transports` — the dropdown's data source for
/// Settings → User → My Identities. Returns ONLY plugin-registered
/// `[transport]` declarations.
///
/// Why no built-ins: the only candidates would be `web` and
/// `voice`, but the controller is already authenticated to the
/// SPA — there's no inbound-resolution surface for either. Adding
/// them as identity options would tell the operator "your already-
/// authenticated session needs an identifier to resolve to you,"
/// which is incoherent. The whole point of My Identities is
/// bridging EXTERNAL transports (Signal, future WhatsApp, etc.)
/// onto the controller principal so an inbound message bypasses
/// cold-contact. An empty list is the correct empty state — the
/// SPA renders an "install a transport plugin" hint.
#[utoipa::path(
    get,
    path = "/api/admin/me/transports",
    responses((status = 200, description = "Transports available for identity binding", body = AvailableTransportsResponse)),
    security(("bearer_jwt" = [])),
    tag = "my-identities"
)]
pub async fn list_transports_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<AvailableTransportsResponse>, ApiError> {
    let registry = state.plugin_host.registry();
    let out: Vec<AvailableTransportView> = registry
        .all_transports()
        .into_iter()
        .map(|t| AvailableTransportView {
            handle_placeholder: handle_placeholder_for(&t.transport_id),
            label: pretty_label_for(&t.transport_id),
            plugin_id: Some(t.plugin_id),
            id: t.transport_id,
        })
        .collect();
    Ok(Json(AvailableTransportsResponse { transports: out }))
}

/// Title-case fallback so `signal` renders as `Signal` even when
/// the registry doesn't carry the plugin's display name. Plugins
/// that want a different label (e.g. "Microsoft Teams") will
/// eventually override via a `[transport].label` manifest knob —
/// the existing `[transport]` block doesn't carry one yet, so we
/// title-case as a stop-gap.
fn pretty_label_for(transport_id: &str) -> String {
    let mut chars = transport_id.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => transport_id.to_owned(),
    }
}

/// Best-effort handle hint — the operator sees this as the form
/// field's placeholder. Phone-number transports default to E.164;
/// everything else falls back to a generic "handle" prompt.
fn handle_placeholder_for(transport_id: &str) -> String {
    match transport_id {
        "signal" | "whatsapp" | "sms" => "+15551234".into(),
        "email" => "you@example.com".into(),
        _ => "handle".into(),
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/me/identifiers",
    responses((status = 200, description = "Controller's identifiers", body = MyIdentitiesResponse)),
    security(("bearer_jwt" = [])),
    tag = "my-identities"
)]
pub async fn list_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<MyIdentitiesResponse>, ApiError> {
    let pid = controller_principal_id(&state.db)?;
    let store = PrincipalStore::new(&state.db);
    let identifiers = match store.get(&pid).map_err(db_err)? {
        Some(p) => p
            .identifiers
            .into_iter()
            .map(|i| IdentifierView {
                transport: i.transport,
                handle: i.handle,
            })
            .collect(),
        None => Vec::new(),
    };
    Ok(Json(MyIdentitiesResponse {
        controller_principal_id: pid.as_str().to_owned(),
        identifiers,
    }))
}

#[utoipa::path(
    post,
    path = "/api/admin/me/identifiers",
    request_body = AddIdentifierRequest,
    responses(
        (status = 200, description = "Added", body = MyIdentitiesResponse),
        (status = 400, description = "Empty transport / handle, or duplicate"),
        (status = 409, description = "Identifier is already claimed by another principal"),
    ),
    security(("bearer_jwt" = [])),
    tag = "my-identities"
)]
pub async fn add_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<AddIdentifierRequest>,
) -> Result<Json<MyIdentitiesResponse>, ApiError> {
    let transport = req.transport.trim().to_lowercase();
    let handle = req.handle.trim().to_owned();
    if transport.is_empty() || handle.is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "missing_field",
            message: "transport and handle must both be non-empty".into(),
        });
    }

    let pid = controller_principal_id(&state.db)?;
    let store = PrincipalStore::new(&state.db);
    let new_id = Identifier {
        transport: transport.clone(),
        handle: handle.clone(),
    };

    // Refuse if some OTHER principal already claims this identifier;
    // that's a transport-resolver collision the operator must fix
    // before the controller can claim it.
    if let Some(existing) = store.find_by_identifier(&new_id).map_err(db_err)? {
        if existing.id != pid {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                code: "identifier_claimed",
                message: format!(
                    "{}:{} is already attached to principal '{}'",
                    transport,
                    handle,
                    existing.id.as_str()
                ),
            });
        }
    }

    let now = chrono::Utc::now().timestamp();
    let mut row = match store.get(&pid).map_err(db_err)? {
        Some(p) => p,
        None => {
            // Lazy-create. Controller TrustLevel doesn't carry data in
            // the variant, so we can populate the minimal row here.
            Principal {
                id: pid.clone(),
                identifiers: Vec::new(),
                trust_level: TrustLevel::Controller,
                resolved_by: Vec::new(),
                metadata: serde_json::Value::Object(serde_json::Map::new()),
                first_seen: now,
                last_seen: Some(now),
                controller_notes: None,
            }
        }
    };

    if row.identifiers.contains(&new_id) {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "duplicate_identifier",
            message: format!("{transport}:{handle} is already on the controller"),
        });
    }
    row.identifiers.push(new_id.clone());
    row.last_seen = Some(now);
    store.upsert(&row).map_err(db_err)?;

    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "principals",
        pid.as_str(),
        None,
        Some(&serde_json::json!({
            "added_identifier": format!("{transport}:{handle}"),
        })),
    );

    Ok(Json(MyIdentitiesResponse {
        controller_principal_id: pid.as_str().to_owned(),
        identifiers: row
            .identifiers
            .into_iter()
            .map(|i| IdentifierView {
                transport: i.transport,
                handle: i.handle,
            })
            .collect(),
    }))
}

#[utoipa::path(
    delete,
    path = "/api/admin/me/identifiers/{transport}/{handle}",
    params(
        ("transport" = String, Path, description = "Transport name, e.g. signal"),
        ("handle"    = String, Path, description = "Per-transport handle"),
    ),
    responses(
        (status = 200, description = "Removed", body = MyIdentitiesResponse),
        (status = 404, description = "Identifier not on the controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "my-identities"
)]
pub async fn delete_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath((transport, handle)): AxumPath<(String, String)>,
) -> Result<Json<MyIdentitiesResponse>, ApiError> {
    let pid = controller_principal_id(&state.db)?;
    let store = PrincipalStore::new(&state.db);

    let mut row = store.get(&pid).map_err(db_err)?.ok_or(ApiError {
        status: StatusCode::NOT_FOUND,
        code: "no_identifier",
        message: format!("controller has no identifier {transport}:{handle}",),
    })?;
    let target = Identifier {
        transport: transport.to_lowercase(),
        handle: handle.clone(),
    };
    let before_len = row.identifiers.len();
    row.identifiers
        .retain(|i| !(i.transport == target.transport && i.handle == target.handle));
    if row.identifiers.len() == before_len {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "no_identifier",
            message: format!("controller has no identifier {transport}:{handle}",),
        });
    }
    row.last_seen = Some(chrono::Utc::now().timestamp());
    store.upsert(&row).map_err(db_err)?;

    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "principals",
        pid.as_str(),
        Some(&serde_json::json!({
            "removed_identifier": format!("{}:{}", target.transport, target.handle),
        })),
        None,
    );

    Ok(Json(MyIdentitiesResponse {
        controller_principal_id: pid.as_str().to_owned(),
        identifiers: row
            .identifiers
            .into_iter()
            .map(|i| IdentifierView {
                transport: i.transport,
                handle: i.handle,
            })
            .collect(),
    }))
}

fn db_err(e: execlaw_core::DbError) -> ApiError {
    ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "db_error",
        message: e.to_string(),
    }
}

#[allow(dead_code)]
fn _force_principal_id_into_scope(p: PrincipalId) -> PrincipalId {
    p
}

pub fn my_identities_router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/admin/me/identifiers",
            get(list_handler).post(add_handler),
        )
        .route(
            "/api/admin/me/identifiers/{transport}/{handle}",
            axum::routing::delete(delete_handler),
        )
        .route("/api/admin/me/transports", get(list_transports_handler))
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
    async fn list_starts_empty_for_freshly_setup_controller() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/me/identifiers")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["identifiers"].as_array().unwrap().len(), 0);
        assert!(
            v["controller_principal_id"]
                .as_str()
                .unwrap()
                .starts_with("controller-")
        );
    }

    #[tokio::test]
    async fn add_then_list_then_delete_round_trip() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;

        // Add
        let body = serde_json::json!({"transport": "Signal", "handle": "+15551234"});
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/me/identifiers")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let ids = v["identifiers"].as_array().unwrap();
        assert_eq!(ids.len(), 1);
        // Transport is normalized to lowercase.
        assert_eq!(ids[0]["transport"], "signal");
        assert_eq!(ids[0]["handle"], "+15551234");

        // Delete
        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/api/admin/me/identifiers/signal/%2B15551234")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["identifiers"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn add_rejects_duplicate_with_400() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let body = serde_json::json!({"transport": "email", "handle": "me@example.com"});
        let mk_req = || {
            Request::builder()
                .method(Method::POST)
                .uri("/api/admin/me/identifiers")
                .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let resp = app.clone().oneshot(mk_req()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app.oneshot(mk_req()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn add_rejects_empty_fields() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let body = serde_json::json!({"transport": "", "handle": "x"});
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/me/identifiers")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_unknown_404s() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/api/admin/me/identifiers/signal/%2B15551234")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ---- Dynamic transport list (2026-05-04) -----------------------

    #[tokio::test]
    async fn list_transports_is_empty_when_no_plugin_transport_registered() {
        // Fresh test fixture: no plugin transports installed.
        // Endpoint returns an empty list — the SPA renders the
        // "install a transport plugin" hint. Deliberately no
        // built-in entries: the controller is already
        // authenticated to the web shell, so `web` / `voice` would
        // be incoherent identity surfaces (you don't bind your own
        // already-authenticated session as an external identity).
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/me/transports")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v["transports"].as_array().unwrap().len(),
            0,
            "fresh install must surface no transport options",
        );
    }

    #[tokio::test]
    async fn list_transports_surfaces_plugin_registered_signal_transport() {
        use execlaw_plugin_sdk::PluginManifest;
        let state = test_app_state();
        // Simulate enable: register the Signal plugin's
        // `[transport]` declaration directly on the registry. The
        // plugin install path normally does this; we bypass the
        // staging step for test isolation.
        let manifest = PluginManifest::parse(
            r#"
[plugin]
id = "signal"
name = "Signal"
version = "0.1.0"

[transport]
transport_id = "signal"
supports_groups = true
supports_attachments = true
"#,
        )
        .unwrap();
        state.plugin_host.registry().enable(&manifest).unwrap();

        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/me/transports")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let transports = v["transports"].as_array().unwrap();
        let ids: Vec<&str> = transports
            .iter()
            .map(|t| t.get("id").and_then(|v| v.as_str()).unwrap())
            .collect();
        assert!(ids.contains(&"signal"), "got {ids:?}");
        // The Signal entry carries plugin_id = "signal" so the SPA
        // can attribute it to the plugin in the dropdown chip.
        let signal = transports.iter().find(|t| t["id"] == "signal").unwrap();
        assert_eq!(signal["plugin_id"], "signal");
        // Title-cased label fallback so the dropdown reads "Signal"
        // not "signal".
        assert_eq!(signal["label"], "Signal");
        // Phone-shaped placeholder.
        assert_eq!(signal["handle_placeholder"], "+15551234");
    }

}
