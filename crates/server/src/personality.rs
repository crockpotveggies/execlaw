//! Settings → Personality — operator-editable system-prompt fields
//! (Phase 9, MIGRATION_PLAN §5.5).
//!
//! Two-level scope: a single `default` row + sparse `conversation`
//! overrides keyed by conversation_id. The default row is seeded by
//! migration 0013 so /api/admin/personality always returns something.
//!
//! Routes:
//!   * `GET    /api/admin/personality`                    — default + every override
//!   * `GET    /api/admin/personality/default`            — default-scope row only
//!   * `PUT    /api/admin/personality/default`            — upsert default
//!     (Controller-only, audit-logged, bumps version)
//!   * `GET    /api/admin/personality/conversation/{cid}` — fetch one override (404 if absent)
//!   * `PUT    /api/admin/personality/conversation/{cid}` — upsert override
//!     (Controller-only, audit-logged)
//!   * `DELETE /api/admin/personality/conversation/{cid}` — drop override
//!     (Controller-only, audit-logged)
//!   * `GET    /api/admin/personality/preview?conversation_id=...`
//!     — render the composed system prompt the agent will see.
//!     Controller + Operator can preview; useful for "why is the
//!     agent saying X" debugging.

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use execlaw_core::audit::AuditStore;
use execlaw_core::personality::{
    PersonalityError, PersonalityField, PersonalityRow, PersonalityScopeKind, PersonalityStore,
    PersonalityUpsert, compose_system_prompt,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct PersonalityView {
    pub scope_kind: String,
    pub scope_ref: String,
    pub display_name: String,
    pub role: String,
    pub tone: String,
    pub communication_style: String,
    pub initiative: String,
    pub about_agent: String,
    pub about_controller: String,
    pub custom_instructions: String,
    pub voice_id: Option<String>,
    /// Field names ("display_name", "tone", ...) the operator
    /// explicitly set on this scope. Default-scope rows list every
    /// field; conversation-scope rows list only the overridden ones
    /// so the merge can fall through to default for the rest.
    pub override_fields: Vec<String>,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<&PersonalityRow> for PersonalityView {
    fn from(r: &PersonalityRow) -> Self {
        let mut override_fields: Vec<String> = r
            .override_fields
            .iter()
            .map(|f| f.as_str().to_owned())
            .collect();
        // Stable ordering so the SPA doesn't see flapping arrays.
        override_fields.sort();
        Self {
            scope_kind: r.scope_kind.as_str().to_owned(),
            scope_ref: r.scope_ref.clone(),
            display_name: r.display_name.clone(),
            role: r.role.clone(),
            tone: r.tone.clone(),
            communication_style: r.communication_style.clone(),
            initiative: r.initiative.clone(),
            about_agent: r.about_agent.clone(),
            about_controller: r.about_controller.clone(),
            custom_instructions: r.custom_instructions.clone(),
            voice_id: r.voice_id.clone(),
            override_fields,
            version: r.version,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PersonalityListResponse {
    pub default: PersonalityView,
    pub overrides: Vec<PersonalityView>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpsertPersonalityRequest {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub tone: String,
    #[serde(default)]
    pub communication_style: String,
    #[serde(default)]
    pub initiative: String,
    #[serde(default)]
    pub about_agent: String,
    #[serde(default)]
    pub about_controller: String,
    #[serde(default)]
    pub custom_instructions: String,
    #[serde(default)]
    pub voice_id: Option<String>,
    /// For conversation-scope upserts ONLY: the field names the
    /// operator wants to override at this scope. Field names not in
    /// this list inherit from `default`. Ignored for default-scope
    /// upserts (every field is implicitly overridden at the default
    /// level).
    #[serde(default)]
    pub override_fields: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PersonalityPreviewResponse {
    /// Effective conversation id — echoes back the input so the SPA
    /// can confirm. Empty string when previewing the default scope.
    pub conversation_id: String,
    pub system_prompt: String,
}

#[derive(Debug, Deserialize)]
pub struct PreviewQuery {
    /// Optional conversation id. Omitting it previews the default-only
    /// composition (i.e., what a brand-new conversation would see).
    pub conversation_id: Option<String>,
}

impl From<PersonalityError> for ApiError {
    fn from(err: PersonalityError) -> Self {
        match err {
            PersonalityError::Db(e) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "db_error",
                message: e.to_string(),
            },
            PersonalityError::Sqlite(e) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "db_error",
                message: e.to_string(),
            },
            PersonalityError::Invalid(msg) => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_personality",
                message: msg,
            },
            PersonalityError::CannotDeleteDefault => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "cannot_delete_default",
                message: "the default personality scope cannot be deleted".into(),
            },
            PersonalityError::NotFound { kind, rref } => ApiError {
                status: StatusCode::NOT_FOUND,
                code: "personality_not_found",
                message: format!("personality scope not found: {kind}/{rref}"),
            },
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/personality",
    responses((status = 200, description = "Default + every override", body = PersonalityListResponse)),
    security(("bearer_jwt" = [])),
    tag = "personality"
)]
pub async fn list_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<PersonalityListResponse>, ApiError> {
    let store = PersonalityStore::new(&state.db);
    let default = store.get_default().map_err(ApiError::from)?;
    let overrides = store.list_overrides().map_err(ApiError::from)?;
    Ok(Json(PersonalityListResponse {
        default: (&default).into(),
        overrides: overrides.iter().map(PersonalityView::from).collect(),
    }))
}

#[utoipa::path(
    get,
    path = "/api/admin/personality/default",
    responses((status = 200, description = "Default-scope row", body = PersonalityView)),
    security(("bearer_jwt" = [])),
    tag = "personality"
)]
pub async fn get_default_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<PersonalityView>, ApiError> {
    let store = PersonalityStore::new(&state.db);
    let row = store.get_default().map_err(ApiError::from)?;
    Ok(Json((&row).into()))
}

#[utoipa::path(
    put,
    path = "/api/admin/personality/default",
    request_body = UpsertPersonalityRequest,
    responses(
        (status = 200, description = "Upserted default", body = PersonalityView),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "personality"
)]
pub async fn upsert_default_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<UpsertPersonalityRequest>,
) -> Result<Json<PersonalityView>, ApiError> {
    require_controller(&state, &user)?;
    do_upsert(
        &state,
        &user,
        PersonalityScopeKind::Default,
        "".to_owned(),
        req,
    )
}

#[utoipa::path(
    get,
    path = "/api/admin/personality/conversation/{conversation_id}",
    params(("conversation_id" = String, Path, description = "Conversation id")),
    responses(
        (status = 200, description = "Override row", body = PersonalityView),
        (status = 404, description = "No override exists for this conversation"),
    ),
    security(("bearer_jwt" = [])),
    tag = "personality"
)]
pub async fn get_conversation_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
    AxumPath(conversation_id): AxumPath<String>,
) -> Result<Json<PersonalityView>, ApiError> {
    let store = PersonalityStore::new(&state.db);
    let row = store
        .get(PersonalityScopeKind::Conversation, &conversation_id)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "personality_not_found",
            message: format!("no personality override for conversation '{conversation_id}'"),
        })?;
    Ok(Json((&row).into()))
}

#[utoipa::path(
    put,
    path = "/api/admin/personality/conversation/{conversation_id}",
    request_body = UpsertPersonalityRequest,
    params(("conversation_id" = String, Path, description = "Conversation id")),
    responses(
        (status = 200, description = "Upserted override", body = PersonalityView),
        (status = 400, description = "Empty conversation_id"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "personality"
)]
pub async fn upsert_conversation_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(conversation_id): AxumPath<String>,
    Json(req): Json<UpsertPersonalityRequest>,
) -> Result<Json<PersonalityView>, ApiError> {
    require_controller(&state, &user)?;
    if conversation_id.trim().is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "missing_conversation_id",
            message: "conversation_id path segment is required".into(),
        });
    }
    do_upsert(
        &state,
        &user,
        PersonalityScopeKind::Conversation,
        conversation_id,
        req,
    )
}

#[utoipa::path(
    delete,
    path = "/api/admin/personality/conversation/{conversation_id}",
    params(("conversation_id" = String, Path, description = "Conversation id")),
    responses(
        (status = 200, description = "Override dropped"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "No override existed"),
    ),
    security(("bearer_jwt" = [])),
    tag = "personality"
)]
pub async fn delete_conversation_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(conversation_id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let store = PersonalityStore::new(&state.db);
    let prior = store
        .get(PersonalityScopeKind::Conversation, &conversation_id)
        .map_err(ApiError::from)?;
    let removed = store
        .delete(PersonalityScopeKind::Conversation, &conversation_id)
        .map_err(ApiError::from)?;
    if !removed {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "personality_not_found",
            message: format!("no personality override for conversation '{conversation_id}'"),
        });
    }
    if let Some(prior) = prior {
        let _ = AuditStore::new(&state.db).insert(
            &user.user_id,
            "config_personality",
            &format!("conversation:{conversation_id}"),
            Some(&serde_json::json!({
                "version": prior.version,
                "override_fields": prior
                    .override_fields
                    .iter()
                    .map(|f| f.as_str())
                    .collect::<Vec<_>>(),
            })),
            None,
        );
    }
    Ok(StatusCode::OK)
}

#[utoipa::path(
    get,
    path = "/api/admin/personality/preview",
    params(("conversation_id" = Option<String>, Query, description = "Optional conversation id")),
    responses((status = 200, description = "Composed system prompt", body = PersonalityPreviewResponse)),
    security(("bearer_jwt" = [])),
    tag = "personality"
)]
pub async fn preview_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
    Query(q): Query<PreviewQuery>,
) -> Result<Json<PersonalityPreviewResponse>, ApiError> {
    let store = PersonalityStore::new(&state.db);
    let cid = q.conversation_id.as_deref().filter(|s| !s.is_empty());
    let prompt = compose_system_prompt(&store, cid).map_err(ApiError::from)?;
    Ok(Json(PersonalityPreviewResponse {
        conversation_id: cid.unwrap_or("").to_owned(),
        system_prompt: prompt,
    }))
}

fn do_upsert(
    state: &AppState,
    user: &AuthedUser,
    scope_kind: PersonalityScopeKind,
    scope_ref: String,
    req: UpsertPersonalityRequest,
) -> Result<Json<PersonalityView>, ApiError> {
    // Translate the wire-format string list into the typed enum set
    // the store expects. Unknown names are silently dropped so a
    // future field on the SPA side can roll out before the server
    // recognises it.
    let override_fields: HashSet<PersonalityField> = req
        .override_fields
        .iter()
        .filter_map(|s| PersonalityField::parse(s))
        .collect();

    let store = PersonalityStore::new(&state.db);
    let prior = store.get(scope_kind, &scope_ref).map_err(ApiError::from)?;

    let now = chrono::Utc::now().timestamp();
    let row = store
        .upsert(
            &PersonalityUpsert {
                scope_kind,
                scope_ref: scope_ref.clone(),
                display_name: req.display_name.clone(),
                role: req.role.clone(),
                tone: req.tone.clone(),
                communication_style: req.communication_style.clone(),
                initiative: req.initiative.clone(),
                about_agent: req.about_agent.clone(),
                about_controller: req.about_controller.clone(),
                custom_instructions: req.custom_instructions.clone(),
                voice_id: req.voice_id.clone(),
                override_fields,
            },
            now,
        )
        .map_err(ApiError::from)?;

    let row_id = match scope_kind {
        PersonalityScopeKind::Default => "default".to_owned(),
        PersonalityScopeKind::Conversation => format!("conversation:{scope_ref}"),
    };
    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "config_personality",
        &row_id,
        prior
            .as_ref()
            .map(|r| {
                serde_json::json!({
                    "version": r.version,
                    "display_name": r.display_name,
                    "tone": r.tone,
                })
            })
            .as_ref(),
        Some(&serde_json::json!({
            "version": row.version,
            "display_name": row.display_name,
            "tone": row.tone,
        })),
    );

    Ok(Json((&row).into()))
}

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
            code: "controller_only",
            message: "only a Controller can change personality".into(),
        }),
    }
}

pub fn personality_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/personality", get(list_handler))
        .route(
            "/api/admin/personality/default",
            get(get_default_handler).put(upsert_default_handler),
        )
        .route(
            "/api/admin/personality/conversation/{conversation_id}",
            get(get_conversation_handler)
                .put(upsert_conversation_handler)
                .delete(delete_conversation_handler),
        )
        .route("/api/admin/personality/preview", get(preview_handler))
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

    fn default_upsert_body() -> serde_json::Value {
        serde_json::json!({
            "display_name": "Earl",
            "role": "Personal assistant",
            "tone": "Concise, practical",
            "communication_style": "Single-sentence replies",
            "initiative": "Ask before scheduling",
            "about_agent": "I never sleep.",
            "about_controller": "Justin lives in NYC.",
            "custom_instructions": "Refuse legal-doc summaries.",
            "voice_id": "bf_emma",
        })
    }

    #[tokio::test]
    async fn list_returns_seeded_default_and_no_overrides() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/personality")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["default"]["scope_kind"], "default");
        assert_eq!(v["default"]["display_name"], "execlaw");
        assert_eq!(v["overrides"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn put_default_bumps_version_and_persists() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/personality/default")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(default_upsert_body().to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["display_name"], "Earl");
        assert_eq!(v["version"], 2, "seeded v=1 then upsert bumps to v=2");
    }

    #[tokio::test]
    async fn put_conversation_override_field_inheritance() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;

        // Step 1: configure a meaningful default.
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/personality/default")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(default_upsert_body().to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Step 2: PUT a conversation override that only touches `tone`.
        let body = serde_json::json!({
            "display_name": "ignored — not in override_fields",
            "tone": "Pirate",
            "override_fields": ["tone"],
        });
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/personality/conversation/conv-1")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Step 3: preview the composed prompt — Earl + Pirate.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/personality/preview?conversation_id=conv-1")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let prompt = v["system_prompt"].as_str().unwrap();
        assert!(
            prompt.contains("Name: Earl"),
            "default identity flows through: {prompt}"
        );
        assert!(
            prompt.contains("# Tone\nPirate"),
            "override wins for listed field: {prompt}"
        );
    }

    #[tokio::test]
    async fn delete_conversation_then_404_on_second_delete() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;

        // Create override.
        let body = serde_json::json!({
            "tone": "Pirate",
            "override_fields": ["tone"],
        });
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/personality/conversation/conv-x")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // First delete → 200.
        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/api/admin/personality/conversation/conv-x")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Second delete → 404.
        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/api/admin/personality/conversation/conv-x")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn preview_without_conversation_uses_default_only() {
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/personality/preview")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let prompt = v["system_prompt"].as_str().unwrap();
        // Seeded default has display_name=execlaw and role=Personal assistant.
        assert!(prompt.contains("Name: execlaw"));
        assert!(prompt.contains("# Identity"));
        assert!(prompt.contains("<!-- personality v="));
    }

    #[tokio::test]
    async fn put_conversation_with_empty_id_is_rejected() {
        // Empty conversation id can't even reach the handler — Axum's
        // path matcher rejects a missing segment with 404. We test the
        // whitespace-only case which DOES reach the handler.
        let app = build_router(test_app_state());
        let tok = setup_controller_token(&app).await;
        let body = serde_json::json!({"tone": "x", "override_fields": ["tone"]});
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/personality/conversation/%20")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
