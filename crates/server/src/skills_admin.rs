//! Admin HTTP surface for the top-level Skills page (Phase B.3,
//! 2026-05-03).
//!
//! Routes:
//!   * `GET  /api/admin/skills`              — list visible skills
//!   * `GET  /api/admin/skills/{name}`       — full skill detail
//!   * `POST /api/admin/skills/{name}/promote` (controller-only)
//!   * `POST /api/admin/skills/{name}/archive` (controller-only)
//!
//! Backed by `execlaw_skills::SkillStore`. Constructed per-request
//! from the shared `state.db` handle (the store is a thin wrapper
//! around the `Database` so creation is cheap and there's no need to
//! plumb another field through `AppState`).

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use execlaw_core::skills_config::{SkillsConfig, SkillsConfigStore, SkillsConfigUpdate};
use execlaw_skills::{NewSkillVersion, ProposalId, ProposalState, SkillStore, Strictness};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Compact list entry — what the SPA renders in the left rail.
#[derive(Debug, Serialize, ToSchema)]
pub struct SkillListEntry {
    pub name: String,
    pub description: String,
    /// `trial` | `stable` | `archived`
    pub state: String,
    pub version: u32,
    /// `authored` | `shipped` | `registered`
    pub registration_kind: String,
    pub source: String,
    pub owning_plugin_id: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SkillListResponse {
    pub skills: Vec<SkillListEntry>,
}

/// Detail view — full body + frontmatter + bundled-resource paths.
#[derive(Debug, Serialize, ToSchema)]
pub struct SkillDetail {
    pub name: String,
    pub description: String,
    pub state: String,
    pub registration_kind: String,
    pub source: String,
    pub owning_plugin_id: Option<String>,
    pub current_version: u32,
    pub body_md: String,
    pub frontmatter_json: String,
    pub authored_by: String,
    pub authored_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub archived_at: Option<i64>,
    pub resource_paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// When `true`, archived skills appear in the list.
    /// Default `false` — UI shows them via a checkbox toggle.
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PromoteRequest {
    #[serde(default)]
    pub notes: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/admin/skills",
    responses((status = 200, description = "Visible skills", body = SkillListResponse)),
    security(("bearer_jwt" = [])),
    tag = "skills"
)]
pub async fn list_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
    Query(q): Query<ListQuery>,
) -> Result<Json<SkillListResponse>, ApiError> {
    let store = SkillStore::new(state.db.clone());
    // The store's `list_index` already filters out archived. For
    // include_archived=true we pull every row by name range via a
    // direct query — simpler than expanding the store's API surface
    // for a UI affordance.
    let entries = if q.include_archived {
        list_all_for_admin(&store).map_err(skill_err)?
    } else {
        let idx = store.list_index().map_err(skill_err)?;
        // Hydrate each entry with source / kind / owning_plugin / updated_at via a get().
        let mut out = Vec::with_capacity(idx.len());
        for e in idx {
            if let Some(s) = store.get(&e.name).map_err(skill_err)? {
                out.push(SkillListEntry {
                    name: s.name,
                    description: s.current_version.description,
                    state: s.state.as_str().to_owned(),
                    version: s.current_version.version,
                    registration_kind: s.registration_kind.as_str().to_owned(),
                    source: s.source,
                    owning_plugin_id: s.owning_plugin_id,
                    updated_at: s.updated_at,
                });
            }
        }
        out
    };
    Ok(Json(SkillListResponse { skills: entries }))
}

/// Pull every row including archived; used when include_archived=true.
fn list_all_for_admin(
    store: &SkillStore,
) -> Result<Vec<SkillListEntry>, execlaw_skills::SkillError> {
    use rusqlite::params;
    let names: Vec<String> = store
        .db()
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT name FROM state_skills ORDER BY name")?;
            let rows = stmt
                .query_map(params![], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .map_err(execlaw_skills::SkillError::Db)?;
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        if let Some(s) = store.get(&name)? {
            out.push(SkillListEntry {
                name: s.name,
                description: s.current_version.description,
                state: s.state.as_str().to_owned(),
                version: s.current_version.version,
                registration_kind: s.registration_kind.as_str().to_owned(),
                source: s.source,
                owning_plugin_id: s.owning_plugin_id,
                updated_at: s.updated_at,
            });
        }
    }
    Ok(out)
}

#[utoipa::path(
    get,
    path = "/api/admin/skills/{name}",
    params(("name" = String, Path, description = "Full skill name (`<namespace>/<segment>`)")),
    responses(
        (status = 200, description = "Skill detail", body = SkillDetail),
        (status = 404, description = "Skill not found")
    ),
    security(("bearer_jwt" = [])),
    tag = "skills"
)]
pub async fn get_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<SkillDetail>, ApiError> {
    let store = SkillStore::new(state.db.clone());
    let s = store
        .get(&name)
        .map_err(skill_err)?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "skill_not_found",
            message: format!("no skill named '{name}'"),
        })?;
    // Resource paths come from the view (which only returns paths for
    // non-archived skills); for archived skills we skip them.
    let resource_paths = if matches!(s.state, execlaw_skills::SkillState::Archived) {
        Vec::new()
    } else {
        store
            .view(&name)
            .map_err(skill_err)?
            .map(|v| v.resource_paths)
            .unwrap_or_default()
    };
    Ok(Json(SkillDetail {
        name: s.name,
        description: s.current_version.description.clone(),
        state: s.state.as_str().to_owned(),
        registration_kind: s.registration_kind.as_str().to_owned(),
        source: s.source.clone(),
        owning_plugin_id: s.owning_plugin_id.clone(),
        current_version: s.current_version.version,
        body_md: s.current_version.body_md.clone(),
        frontmatter_json: s.current_version.frontmatter_json.clone(),
        authored_by: s.current_version.authored_by.clone(),
        authored_at: s.current_version.authored_at,
        created_at: s.created_at,
        updated_at: s.updated_at,
        archived_at: s.archived_at,
        resource_paths,
    }))
}

#[utoipa::path(
    post,
    path = "/api/admin/skills/{name}/promote",
    request_body = PromoteRequest,
    params(("name" = String, Path, description = "Full skill name")),
    responses(
        (status = 200, description = "Promoted (or no-op if already stable)", body = SkillDetail),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "Skill not found")
    ),
    security(("bearer_jwt" = [])),
    tag = "skills"
)]
pub async fn promote_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(name): AxumPath<String>,
    Json(req): Json<PromoteRequest>,
) -> Result<Json<SkillDetail>, ApiError> {
    require_controller(&state, &user)?;
    let store = SkillStore::new(state.db.clone());
    let now_ms = chrono::Utc::now().timestamp() * 1000;
    store.promote(&name, req.notes, now_ms).map_err(skill_err)?;
    get_handler(State(state), user, AxumPath(name)).await
}

#[utoipa::path(
    post,
    path = "/api/admin/skills/{name}/archive",
    params(("name" = String, Path, description = "Full skill name")),
    responses(
        (status = 200, description = "Archived (or no-op if already archived)", body = SkillDetail),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "Skill not found")
    ),
    security(("bearer_jwt" = [])),
    tag = "skills"
)]
pub async fn archive_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<SkillDetail>, ApiError> {
    require_controller(&state, &user)?;
    let store = SkillStore::new(state.db.clone());
    let now_ms = chrono::Utc::now().timestamp() * 1000;
    store.archive(&name, now_ms).map_err(skill_err)?;
    get_handler(State(state), user, AxumPath(name)).await
}

fn skill_err(e: execlaw_skills::SkillError) -> ApiError {
    use execlaw_skills::SkillError::*;
    match e {
        NotFound(n) => ApiError {
            status: StatusCode::NOT_FOUND,
            code: "skill_not_found",
            message: n,
        },
        InvalidName(n) => ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_name",
            message: n,
        },
        AlreadyExists(n) => ApiError {
            status: StatusCode::CONFLICT,
            code: "already_exists",
            message: n,
        },
        Blocked { findings, fields } => ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "secret_scanner_blocked",
            message: format!("scanner blocked: {findings} finding(s) in {fields:?}"),
        },
        Denied(r) => ApiError {
            status: StatusCode::FORBIDDEN,
            code: "denied",
            message: r,
        },
        other => ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "skill_error",
            message: other.to_string(),
        },
    }
}

fn require_controller(state: &AppState, user: &AuthedUser) -> Result<(), ApiError> {
    use execlaw_core::users::{UserRole, UserStore};
    let row = UserStore::new(&state.db)
        .get_by_id(&user.user_id)
        .map_err(ApiError::from)?;
    if matches!(row.map(|u| u.role), Some(UserRole::Controller)) {
        Ok(())
    } else {
        Err(ApiError {
            status: StatusCode::FORBIDDEN,
            code: "controller_only",
            message: "only a Controller can promote or archive skills".into(),
        })
    }
}

// ----------------------------------------------------------------
// Phase C — config: GET / PUT auto-capture toggle
// ----------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct SkillsConfigView {
    pub auto_capture_enabled: bool,
    pub auto_capture_min_tool_calls: u32,
    pub auto_capture_dry_run: bool,
    pub reuse_update_enabled: bool,
    pub updated_at: i64,
}

impl From<SkillsConfig> for SkillsConfigView {
    fn from(c: SkillsConfig) -> Self {
        Self {
            auto_capture_enabled: c.auto_capture_enabled,
            auto_capture_min_tool_calls: c.auto_capture_min_tool_calls,
            auto_capture_dry_run: c.auto_capture_dry_run,
            reuse_update_enabled: c.reuse_update_enabled,
            updated_at: c.updated_at,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateSkillsConfigRequest {
    pub auto_capture_enabled: Option<bool>,
    pub auto_capture_min_tool_calls: Option<u32>,
    pub auto_capture_dry_run: Option<bool>,
    pub reuse_update_enabled: Option<bool>,
}

pub async fn get_config_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> Result<Json<SkillsConfigView>, ApiError> {
    let cfg = SkillsConfigStore::new(&state.db)
        .get()
        .map_err(ApiError::from)?;
    Ok(Json(cfg.into()))
}

pub async fn update_config_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<UpdateSkillsConfigRequest>,
) -> Result<Json<SkillsConfigView>, ApiError> {
    require_controller(&state, &user)?;
    let update = SkillsConfigUpdate {
        auto_capture_enabled: req.auto_capture_enabled,
        auto_capture_min_tool_calls: req.auto_capture_min_tool_calls,
        auto_capture_dry_run: req.auto_capture_dry_run,
        reuse_update_enabled: req.reuse_update_enabled,
    };
    let now_ms = chrono::Utc::now().timestamp() * 1000;
    let updated = SkillsConfigStore::new(&state.db)
        .update(&update, now_ms)
        .map_err(|e| ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "config_update_rejected",
            message: e.to_string(),
        })?;
    Ok(Json(updated.into()))
}

// ----------------------------------------------------------------
// Phase D.1 — body editor: PUT /api/admin/skills/{name}
// ----------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateSkillBodyRequest {
    pub description: String,
    pub body_md: String,
    #[serde(default = "default_frontmatter_admin")]
    pub frontmatter_json: String,
    #[serde(default)]
    pub promotion_notes: Option<String>,
}

fn default_frontmatter_admin() -> String {
    "{}".into()
}

pub async fn update_body_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(name): AxumPath<String>,
    Json(req): Json<UpdateSkillBodyRequest>,
) -> Result<Json<SkillDetail>, ApiError> {
    require_controller(&state, &user)?;
    let store = SkillStore::new(state.db.clone());
    let now_ms = chrono::Utc::now().timestamp() * 1000;
    let v = NewSkillVersion {
        description: req.description,
        body_md: req.body_md,
        frontmatter_json: req.frontmatter_json,
        authored_by: format!("admin:{}", user.user_id),
        promotion_notes: req.promotion_notes,
    };
    store
        .add_version(&name, v, Strictness::Warn, now_ms)
        .map_err(skill_err)?;
    get_handler(State(state), user, AxumPath(name)).await
}

// ----------------------------------------------------------------
// Phase D.1 — version history: GET /api/admin/skills/{name}/versions
// ----------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct SkillVersionView {
    pub version: u32,
    pub description: String,
    pub body_md: String,
    pub frontmatter_json: String,
    pub authored_by: String,
    pub authored_at: i64,
    pub promotion_notes: Option<String>,
    pub parent_version: Option<u32>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SkillVersionListResponse {
    pub versions: Vec<SkillVersionView>,
}

pub async fn list_versions_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<SkillVersionListResponse>, ApiError> {
    let store = SkillStore::new(state.db.clone());
    let versions = store.list_versions(&name).map_err(skill_err)?;
    if versions.is_empty() {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "skill_not_found",
            message: format!("no skill named '{name}'"),
        });
    }
    // Build a map from version_id -> version number so we can render
    // parent_version_id as an inline parent_version.
    let id_to_num: std::collections::HashMap<i64, u32> =
        versions.iter().map(|v| (v.id.0, v.version)).collect();
    let out: Vec<SkillVersionView> = versions
        .iter()
        .map(|v| SkillVersionView {
            version: v.version,
            description: v.description.clone(),
            body_md: v.body_md.clone(),
            frontmatter_json: v.frontmatter_json.clone(),
            authored_by: v.authored_by.clone(),
            authored_at: v.authored_at,
            promotion_notes: v.promotion_notes.clone(),
            parent_version: v
                .parent_version_id
                .and_then(|p| id_to_num.get(&p.0).copied()),
        })
        .collect();
    Ok(Json(SkillVersionListResponse { versions: out }))
}

// ----------------------------------------------------------------
// Phase D.1 — proposals: list + get + approve + reject
// ----------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct SkillProposalView {
    pub id: i64,
    pub kind: String,
    pub target_skill_id: Option<i64>,
    pub proposed_name: String,
    pub description: String,
    pub body_md: String,
    pub frontmatter_json: String,
    pub source_run_id: String,
    pub trajectory_summary: Option<String>,
    pub tool_calls_observed: u32,
    pub state: String,
    pub promoted_skill_id: Option<i64>,
    pub promoted_version_id: Option<i64>,
    pub created_at: i64,
    pub reviewed_at: Option<i64>,
    pub reviewer: Option<String>,
    pub decision_notes: Option<String>,
}

impl From<execlaw_skills::SkillProposal> for SkillProposalView {
    fn from(p: execlaw_skills::SkillProposal) -> Self {
        Self {
            id: p.id.0,
            kind: p.kind.as_str().to_string(),
            target_skill_id: p.target_skill_id.map(|s| s.0),
            proposed_name: p.proposed_name,
            description: p.description,
            body_md: p.body_md,
            frontmatter_json: p.frontmatter_json,
            source_run_id: p.source_run_id,
            trajectory_summary: p.trajectory_summary,
            tool_calls_observed: p.tool_calls_observed,
            state: p.state.as_str().to_string(),
            promoted_skill_id: p.promoted_skill_id.map(|s| s.0),
            promoted_version_id: p.promoted_version_id.map(|v| v.0),
            created_at: p.created_at,
            reviewed_at: p.reviewed_at,
            reviewer: p.reviewer,
            decision_notes: p.decision_notes,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SkillProposalListResponse {
    pub proposals: Vec<SkillProposalView>,
}

#[derive(Debug, Deserialize)]
pub struct ListProposalsQuery {
    /// `pending` (default), `approved`, `rejected`, `superseded`,
    /// or `all` (omit filter).
    #[serde(default)]
    pub state: Option<String>,
}

pub async fn list_proposals_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
    Query(q): Query<ListProposalsQuery>,
) -> Result<Json<SkillProposalListResponse>, ApiError> {
    let store = SkillStore::new(state.db.clone());
    let filter = match q.state.as_deref() {
        None | Some("pending") => Some(ProposalState::Pending),
        Some("approved") => Some(ProposalState::Approved),
        Some("rejected") => Some(ProposalState::Rejected),
        Some("superseded") => Some(ProposalState::Superseded),
        Some("all") => None,
        Some(other) => {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_state",
                message: format!("unknown state filter: {other}"),
            });
        }
    };
    let rows = store.list_proposals(filter).map_err(skill_err)?;
    Ok(Json(SkillProposalListResponse {
        proposals: rows.into_iter().map(Into::into).collect(),
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReviewProposalRequest {
    #[serde(default)]
    pub notes: Option<String>,
}

pub async fn approve_proposal_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(id): AxumPath<i64>,
    Json(req): Json<ReviewProposalRequest>,
) -> Result<Json<SkillProposalView>, ApiError> {
    require_controller(&state, &user)?;
    let store = SkillStore::new(state.db.clone());
    let now_ms = chrono::Utc::now().timestamp() * 1000;
    store
        .approve_proposal(ProposalId(id), &user.user_id, req.notes, now_ms)
        .map_err(skill_err)?;
    let p = store
        .get_proposal(ProposalId(id))
        .map_err(skill_err)?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "proposal_not_found",
            message: format!("proposal {id} disappeared after approve"),
        })?;
    Ok(Json(p.into()))
}

pub async fn reject_proposal_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    AxumPath(id): AxumPath<i64>,
    Json(req): Json<ReviewProposalRequest>,
) -> Result<Json<SkillProposalView>, ApiError> {
    require_controller(&state, &user)?;
    let store = SkillStore::new(state.db.clone());
    let now_ms = chrono::Utc::now().timestamp() * 1000;
    store
        .reject_proposal(ProposalId(id), &user.user_id, req.notes, now_ms)
        .map_err(skill_err)?;
    let p = store
        .get_proposal(ProposalId(id))
        .map_err(skill_err)?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "proposal_not_found",
            message: format!("proposal {id} disappeared after reject"),
        })?;
    Ok(Json(p.into()))
}

pub fn skills_admin_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/skills", get(list_handler))
        .route(
            "/api/admin/skills/config",
            get(get_config_handler).put(update_config_handler),
        )
        .route("/api/admin/skills/proposals", get(list_proposals_handler))
        .route(
            "/api/admin/skills/proposals/{id}/approve",
            post(approve_proposal_handler),
        )
        .route(
            "/api/admin/skills/proposals/{id}/reject",
            post(reject_proposal_handler),
        )
        .route(
            "/api/admin/skills/{name}",
            get(get_handler).put(update_body_handler),
        )
        .route(
            "/api/admin/skills/{name}/versions",
            get(list_versions_handler),
        )
        .route("/api/admin/skills/{name}/promote", post(promote_handler))
        .route("/api/admin/skills/{name}/archive", post(archive_handler))
}
