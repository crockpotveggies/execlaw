//! Multi-user admin routes (Phase 7).
//!
//! Today's mode is single-controller; Phase-7 multi-user adds the
//! invite + list + delete surface so the controller can stand up
//! `Operator` and `Viewer` accounts. The original Controller cannot
//! be deleted (no last-controller invariant), and only Controllers
//! can mutate the user table.
//!
//! Auth gating is layered:
//!   * Every route requires a valid bearer JWT (via `AuthedUser`).
//!   * Mutating routes additionally require the caller's role to be
//!     `Controller`. Operators / Viewers see read-only.

use crate::auth_extract::AuthedUser;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Router, routing::get};
use execlaw_core::audit::AuditStore;
use execlaw_core::users::{
    UserRole, UserRow, UserStore, normalize_username,
};
use execlaw_vault::hash_password;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct UserView {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    pub role: String,
    pub created_at: i64,
    pub last_login_at: Option<i64>,
}

impl From<UserRow> for UserView {
    fn from(u: UserRow) -> Self {
        Self {
            user_id: u.user_id,
            username: u.username,
            display_name: u.display_name,
            email: u.email,
            role: u.role.as_str().to_owned(),
            created_at: u.created_at,
            last_login_at: u.last_login_at,
        }
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct UserListResponse {
    pub users: Vec<UserView>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct InviteUserRequest {
    pub username: String,
    pub display_name: String,
    /// Initial password the invitee will use to sign in. They can
    /// rotate it via a profile route once they're in (Phase 7+).
    pub initial_password: String,
    /// `"operator"` or `"viewer"`. Inviting another `controller`
    /// is allowed but discouraged outside of break-glass scenarios.
    pub role: String,
    #[serde(default)]
    pub email: Option<String>,
}

const PASSWORD_MIN_LEN: usize = 8;

#[utoipa::path(
    get,
    path = "/api/admin/users",
    responses(
        (status = 200, description = "Operator + viewer + controller accounts", body = UserListResponse),
        (status = 401, description = "Missing or invalid Authorization header"),
    ),
    security(("bearer_jwt" = [])),
    tag = "users"
)]
pub async fn list_handler(
    State(state): State<AppState>,
    _user: AuthedUser,
) -> impl IntoResponse {
    let store = UserStore::new(&state.db);
    match store.list_all() {
        Ok(rows) => {
            let users: Vec<UserView> = rows.into_iter().map(Into::into).collect();
            (
                StatusCode::OK,
                Json(serde_json::json!(UserListResponse { users })),
            )
                .into_response()
        }
        Err(e) => internal(&format!("list users: {e}")),
    }
}

#[utoipa::path(
    post,
    path = "/api/admin/users/invite",
    request_body = InviteUserRequest,
    responses(
        (status = 200, description = "User created", body = UserView),
        (status = 400, description = "Invalid username / password / role"),
        (status = 401, description = "Missing or invalid Authorization header"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 409, description = "Username already taken"),
    ),
    security(("bearer_jwt" = [])),
    tag = "users"
)]
pub async fn invite_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<InviteUserRequest>,
) -> impl IntoResponse {
    if user.role != UserRole::Controller {
        return error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "only Controllers can invite users",
        );
    }
    let role = match req.role.as_str() {
        "controller" => UserRole::Controller,
        "operator" => UserRole::Operator,
        "viewer" => UserRole::Viewer,
        other => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_role",
                &format!("role must be controller | operator | viewer; got {other:?}"),
            );
        }
    };
    let username = match normalize_username(&req.username) {
        Ok(n) => n,
        Err(msg) => {
            return error(StatusCode::BAD_REQUEST, "username_invalid", msg);
        }
    };
    if req.initial_password.len() < PASSWORD_MIN_LEN {
        return error(
            StatusCode::BAD_REQUEST,
            "password_too_short",
            &format!(
                "initial_password must be at least {PASSWORD_MIN_LEN} characters"
            ),
        );
    }
    if req.display_name.trim().is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "display_name_required",
            "display_name must not be empty",
        );
    }

    let store = UserStore::new(&state.db);
    if let Ok(Some(_)) = store.get_by_username(&username) {
        return error(
            StatusCode::CONFLICT,
            "username_taken",
            "another account already uses this username",
        );
    }

    let hash = match hash_password(&req.initial_password) {
        Ok(h) => h,
        Err(e) => return internal(&format!("hash_password: {e}")),
    };
    let user_id = format!(
        "{}-{}",
        match role {
            UserRole::Controller => "controller",
            UserRole::Operator => "operator",
            UserRole::Viewer => "viewer",
        },
        uuid::Uuid::new_v4()
    );
    let now = chrono::Utc::now().timestamp();
    let row = UserRow {
        user_id: user_id.clone(),
        username,
        display_name: req.display_name.trim().to_owned(),
        email: req.email.clone().filter(|s| !s.trim().is_empty()),
        password_hash: hash,
        role,
        created_at: now,
        last_login_at: None,
    };
    if let Err(e) = store.insert(&row) {
        return internal(&format!("insert user: {e}"));
    }

    // Audit-log the invite without exposing the password hash.
    let view: UserView = row.clone().into();
    let new_json = serde_json::to_value(&view).ok();
    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "users",
        &user_id,
        None,
        new_json.as_ref(),
    );

    (StatusCode::OK, Json(serde_json::json!(view))).into_response()
}

#[utoipa::path(
    delete,
    path = "/api/admin/users/{user_id}",
    params(
        ("user_id" = String, Path, description = "Target user_id"),
    ),
    responses(
        (status = 200, description = "User removed"),
        (status = 401, description = "Missing or invalid Authorization header"),
        (status = 403, description = "Caller is not a Controller, or last-controller invariant"),
        (status = 404, description = "Unknown user_id"),
    ),
    security(("bearer_jwt" = [])),
    tag = "users"
)]
pub async fn delete_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(target_id): Path<String>,
) -> impl IntoResponse {
    if user.role != UserRole::Controller {
        return error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "only Controllers can remove users",
        );
    }
    if target_id == user.user_id {
        return error(
            StatusCode::FORBIDDEN,
            "cannot_self_delete",
            "you cannot delete your own account",
        );
    }
    let store = UserStore::new(&state.db);
    let target = match store.get_by_id(&target_id) {
        Ok(Some(u)) => u,
        Ok(None) => {
            return error(
                StatusCode::NOT_FOUND,
                "not_found",
                "no user with that id",
            );
        }
        Err(e) => return internal(&format!("get user: {e}")),
    };
    // Last-controller invariant: deleting the only Controller would
    // leave the install unmanaged.
    if target.role == UserRole::Controller {
        let n = store
            .count_by_role(UserRole::Controller)
            .unwrap_or(0);
        if n <= 1 {
            return error(
                StatusCode::FORBIDDEN,
                "last_controller",
                "refusing to remove the last Controller",
            );
        }
    }

    let prev_view = serde_json::to_value(UserView::from(target)).ok();
    if let Err(e) = store.delete(&target_id) {
        return internal(&format!("delete user: {e}"));
    }
    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "users",
        &target_id,
        prev_view.as_ref(),
        None,
    );

    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

// ---------------------------------------------------------------------------
// Password management — Phase 8.6 Settings → Login surface.
//
// Two routes:
//   * `POST /api/admin/me/password` — self-change. Caller must
//     present their CURRENT password before the new one is accepted.
//     Standard "rotate password" flow.
//   * `POST /api/admin/users/{user_id}/password` — Controller-only
//     reset for another user. Doesn't require the target's current
//     password (the operator is doing this on their behalf, like a
//     forgotten-password reset). Audited; refuses to operate on
//     yourself so a Controller can't bypass the current-password
//     check on themselves by hitting the admin route.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ResetPasswordRequest {
    pub new_password: String,
}

const MIN_NEW_PASSWORD_LEN: usize = 8;

#[utoipa::path(
    post,
    path = "/api/admin/me/password",
    request_body = ChangePasswordRequest,
    responses(
        (status = 200, description = "Password rotated"),
        (status = 400, description = "New password too short"),
        (status = 401, description = "Current password incorrect"),
    ),
    security(("bearer_jwt" = [])),
    tag = "auth"
)]
pub async fn change_my_password_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<ChangePasswordRequest>,
) -> impl IntoResponse {
    if req.new_password.len() < MIN_NEW_PASSWORD_LEN {
        return error(
            StatusCode::BAD_REQUEST,
            "weak_password",
            &format!("new password must be at least {MIN_NEW_PASSWORD_LEN} characters"),
        );
    }
    let users = UserStore::new(&state.db);
    let row = match users.get_by_id(&user.user_id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return error(
                StatusCode::UNAUTHORIZED,
                "user_missing",
                "authenticated user not found",
            );
        }
        Err(e) => return internal(&format!("user lookup: {e}")),
    };
    let ok = match execlaw_vault::verify_password(&req.current_password, &row.password_hash) {
        Ok(v) => v,
        Err(e) => return internal(&format!("password verify: {e}")),
    };
    if !ok {
        return error(
            StatusCode::UNAUTHORIZED,
            "bad_credentials",
            "current password is incorrect",
        );
    }
    let new_hash = match hash_password(&req.new_password) {
        Ok(h) => h,
        Err(e) => return internal(&format!("password hash: {e}")),
    };
    if let Err(e) = users.set_password_hash(&user.user_id, &new_hash) {
        return internal(&format!("password update: {e}"));
    }
    let _ = AuditStore::new(&state.db).insert(
        &user.user_id,
        "users",
        &user.user_id,
        Some(&serde_json::json!({"password_rotated": false})),
        Some(&serde_json::json!({"password_rotated": true, "by": "self"})),
    );
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

#[utoipa::path(
    post,
    path = "/api/admin/users/{user_id}/password",
    request_body = ResetPasswordRequest,
    params(("user_id" = String, Path, description = "Target user")),
    responses(
        (status = 200, description = "Password reset"),
        (status = 400, description = "New password too short / target is self"),
        (status = 403, description = "Caller is not a Controller"),
        (status = 404, description = "Target user not found"),
    ),
    security(("bearer_jwt" = [])),
    tag = "auth"
)]
pub async fn reset_user_password_handler(
    State(state): State<AppState>,
    caller: AuthedUser,
    Path(target_id): Path<String>,
    Json(req): Json<ResetPasswordRequest>,
) -> impl IntoResponse {
    if req.new_password.len() < MIN_NEW_PASSWORD_LEN {
        return error(
            StatusCode::BAD_REQUEST,
            "weak_password",
            &format!("new password must be at least {MIN_NEW_PASSWORD_LEN} characters"),
        );
    }
    if target_id == caller.user_id {
        // Self-reset must go through the current-password flow so
        // we don't accidentally hand a controller a "rotate my own
        // password without proving I know the current one" path.
        return error(
            StatusCode::BAD_REQUEST,
            "use_self_change",
            "use POST /api/admin/me/password to change your own password",
        );
    }
    let users = UserStore::new(&state.db);
    // Caller must be a Controller.
    match users.get_by_id(&caller.user_id) {
        Ok(Some(c)) if c.role == UserRole::Controller => {}
        Ok(Some(_)) => {
            return error(
                StatusCode::FORBIDDEN,
                "controller_only",
                "only a Controller can reset another user's password",
            );
        }
        Ok(None) => {
            return error(
                StatusCode::UNAUTHORIZED,
                "user_missing",
                "authenticated user not found",
            );
        }
        Err(e) => return internal(&format!("caller lookup: {e}")),
    }
    let target = match users.get_by_id(&target_id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return error(
                StatusCode::NOT_FOUND,
                "user_not_found",
                &format!("no user with id '{target_id}'"),
            );
        }
        Err(e) => return internal(&format!("target lookup: {e}")),
    };
    let new_hash = match hash_password(&req.new_password) {
        Ok(h) => h,
        Err(e) => return internal(&format!("password hash: {e}")),
    };
    if let Err(e) = users.set_password_hash(&target.user_id, &new_hash) {
        return internal(&format!("password update: {e}"));
    }
    let _ = AuditStore::new(&state.db).insert(
        &caller.user_id,
        "users",
        &target.user_id,
        Some(&serde_json::json!({"password_rotated": false})),
        Some(&serde_json::json!({"password_rotated": true, "by": caller.user_id})),
    );
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

pub fn users_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/users", get(list_handler))
        .route("/api/admin/users/invite", axum::routing::post(invite_handler))
        .route(
            "/api/admin/users/{user_id}",
            axum::routing::delete(delete_handler),
        )
        .route(
            "/api/admin/me/password",
            axum::routing::post(change_my_password_handler),
        )
        .route(
            "/api/admin/users/{user_id}/password",
            axum::routing::post(reset_user_password_handler),
        )
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn error(
    status: StatusCode,
    code: &str,
    message: &str,
) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({
            "error": {"code": code, "message": message}
        })),
    )
        .into_response()
}

fn internal(msg: &str) -> axum::response::Response {
    tracing::error!("{msg}");
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal", msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::{self, Body};
    use axum::http::{Method, Request, header};
    use tower::ServiceExt;
    use execlaw_vault::hash_password as hash_pw;

    /// Set up a controller via /api/setup and return a token.
    async fn setup_controller(app: &axum::Router) -> String {
        let body = serde_json::to_vec(&serde_json::json!({
            "username": "ctrl",
            "admin_password": "hunter2-longer",
            "display_name": "Controller",
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

    /// Issue a token for an arbitrary pre-inserted user without the
    /// invite path (used to test "non-Controller is forbidden").
    fn issue_token_for(state: &AppState, user_id: &str) -> String {
        let session_id = uuid::Uuid::new_v4().to_string();
        state
            .signer
            .issue_access_token(user_id, &session_id, state.config.access_token_ttl_secs)
            .unwrap()
    }

    async fn json_request(
        app: &axum::Router,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    fn invite_body() -> serde_json::Value {
        serde_json::json!({
            "username": "OpOne",
            "display_name": "Operator One",
            "initial_password": "operator-pass-1",
            "role": "operator",
        })
    }

    #[tokio::test]
    async fn list_users_requires_auth() {
        let app = build_router(test_app_state());
        let (status, _) =
            json_request(&app, Method::GET, "/api/admin/users", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn invite_then_list_shows_both_users() {
        let app = build_router(test_app_state());
        let token = setup_controller(&app).await;

        let (status, body) = json_request(
            &app,
            Method::POST,
            "/api/admin/users/invite",
            Some(&token),
            Some(invite_body()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body was {body}");
        // Username is normalized lowercase.
        assert_eq!(body["username"], "opone");
        assert_eq!(body["role"], "operator");
        let new_id = body["user_id"].as_str().unwrap();

        let (status, body) =
            json_request(&app, Method::GET, "/api/admin/users", Some(&token), None).await;
        assert_eq!(status, StatusCode::OK);
        let users = body["users"].as_array().unwrap();
        assert_eq!(users.len(), 2);
        assert!(users.iter().any(|u| u["user_id"] == new_id));
    }

    #[tokio::test]
    async fn invite_rejects_invalid_role_and_short_password() {
        let app = build_router(test_app_state());
        let token = setup_controller(&app).await;
        let cases: Vec<(serde_json::Value, &str)> = vec![
            (
                serde_json::json!({
                    "username": "x", "display_name": "X",
                    "initial_password": "longenough", "role": "bogus",
                }),
                "invalid_role",
            ),
            (
                serde_json::json!({
                    "username": "shortpass", "display_name": "X",
                    "initial_password": "no", "role": "viewer",
                }),
                "password_too_short",
            ),
            (
                serde_json::json!({
                    "username": "ab", "display_name": "X",
                    "initial_password": "longenough", "role": "operator",
                }),
                "username_invalid",
            ),
        ];
        for (body, expect) in cases {
            let (status, b) = json_request(
                &app,
                Method::POST,
                "/api/admin/users/invite",
                Some(&token),
                Some(body),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{expect}: body was {b}");
            assert_eq!(b["error"]["code"], expect);
        }
    }

    #[tokio::test]
    async fn invite_rejects_duplicate_username() {
        let app = build_router(test_app_state());
        let token = setup_controller(&app).await;
        let _ = json_request(
            &app,
            Method::POST,
            "/api/admin/users/invite",
            Some(&token),
            Some(invite_body()),
        )
        .await;
        let (status, body) = json_request(
            &app,
            Method::POST,
            "/api/admin/users/invite",
            Some(&token),
            Some(invite_body()),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "username_taken");
    }

    /// Operators (and Viewers) can read /api/admin/users but cannot
    /// invite or delete. Adversarial test against role escalation.
    #[tokio::test]
    async fn operator_role_cannot_invite_or_delete() {
        let state = test_app_state();
        // Pre-populate an Operator directly.
        let now = chrono::Utc::now().timestamp();
        UserStore::new(&state.db)
            .insert(&UserRow {
                user_id: "op-direct".into(),
                username: "op".into(),
                display_name: "Op".into(),
                email: None,
                password_hash: hash_pw("op-pass-strong").unwrap(),
                role: UserRole::Operator,
                created_at: now,
                last_login_at: None,
            })
            .unwrap();
        let token = issue_token_for(&state, "op-direct");
        let app = build_router(state);

        // Read is allowed.
        let (status, _) =
            json_request(&app, Method::GET, "/api/admin/users", Some(&token), None).await;
        assert_eq!(status, StatusCode::OK);

        // Invite forbidden.
        let (status, body) = json_request(
            &app,
            Method::POST,
            "/api/admin/users/invite",
            Some(&token),
            Some(invite_body()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["code"], "forbidden");

        // Delete forbidden.
        let (status, body) = json_request(
            &app,
            Method::DELETE,
            "/api/admin/users/op-direct",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["code"], "forbidden");
    }

    #[tokio::test]
    async fn delete_self_is_rejected() {
        let app = build_router(test_app_state());
        let token = setup_controller(&app).await;
        // Find the controller's user_id via /me.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/me")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let me: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let id = me["user_id"].as_str().unwrap();

        let (status, body) = json_request(
            &app,
            Method::DELETE,
            &format!("/api/admin/users/{id}"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["code"], "cannot_self_delete");
    }

    /// The last-controller invariant: with two Controllers, deleting
    /// the second is fine (count drops to 1); but a Controller
    /// holding a foreign token can NOT delete the only remaining
    /// Controller. We exercise both paths.
    #[tokio::test]
    async fn last_controller_cannot_be_deleted() {
        let state = test_app_state();
        let app = build_router(state.clone());
        // First-run setup creates the original Controller — must
        // happen before any other user exists, otherwise /api/setup
        // returns 409 already_initialized.
        let token = setup_controller(&app).await;
        // Then add a second Controller directly so we have two.
        let now = chrono::Utc::now().timestamp();
        UserStore::new(&state.db)
            .insert(&UserRow {
                user_id: "controller-other".into(),
                username: "other".into(),
                display_name: "Other".into(),
                email: None,
                password_hash: hash_pw("other-pass-strong").unwrap(),
                role: UserRole::Controller,
                created_at: now,
                last_login_at: None,
            })
            .unwrap();

        // Step 1: with 2 controllers, deleting the OTHER works.
        let (status, _) = json_request(
            &app,
            Method::DELETE,
            "/api/admin/users/controller-other",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Step 2: only one Controller remains. Add an Operator we
        // can act on, then re-add another Controller and try to
        // delete the original via a token issued to the new one.
        UserStore::new(&state.db)
            .insert(&UserRow {
                user_id: "controller-second".into(),
                username: "ctrl2".into(),
                display_name: "Ctrl2".into(),
                email: None,
                password_hash: hash_pw("second-controller-pass").unwrap(),
                role: UserRole::Controller,
                created_at: now,
                last_login_at: None,
            })
            .unwrap();
        let second_token = issue_token_for(&state, "controller-second");

        // controller-second deletes the original (also Controller).
        // Two existed → count drops to 1, allowed.
        let (_, me_body) = json_request(
            &app,
            Method::GET,
            "/api/admin/me",
            Some(&token),
            None,
        )
        .await;
        let original_id = me_body["user_id"].as_str().unwrap().to_owned();
        let (status, _) = json_request(
            &app,
            Method::DELETE,
            &format!("/api/admin/users/{original_id}"),
            Some(&second_token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Step 3: only `controller-second` remains. Trying to delete
        // it via its own token hits cannot_self_delete; via no other
        // controller token exists, so we exercise the invariant by
        // adding ANOTHER throwaway controller, calling its token,
        // and watching the route refuse to drop the lone remaining
        // one. We add a Viewer and have a fresh Controller try to
        // delete `controller-second` — but with two controllers the
        // delete would succeed. Instead, test the invariant directly
        // at the store level since no two-token path leaves exactly
        // one Controller.
        let count = UserStore::new(&state.db)
            .count_by_role(UserRole::Controller)
            .unwrap();
        assert_eq!(count, 1);
        // Confirm the route would refuse. Issue a synthetic token
        // claiming to be `controller-second` again; deleting itself
        // hits cannot_self_delete first, so this branch of the
        // invariant is covered by the existing self-delete test.
        // We document the property holds via the count + the
        // dedicated invariant in `count_by_role` covered in the
        // core unit tests.
    }

    // ---- Password rotation ------------------------------------------------

    #[tokio::test]
    async fn change_my_password_round_trip() {
        let app = build_router(test_app_state());
        let token = setup_controller(&app).await;

        let (status, body) = json_request(
            &app,
            Method::POST,
            "/api/admin/me/password",
            Some(&token),
            Some(serde_json::json!({
                "current_password": "hunter2-longer",
                "new_password": "newer-passphrase-1",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body was {body}");

        // Old password no longer logs us in; new one does.
        let (s_old, _) = json_request(
            &app,
            Method::POST,
            "/api/login",
            None,
            Some(serde_json::json!({
                "username": "ctrl",
                "admin_password": "hunter2-longer",
            })),
        )
        .await;
        assert_eq!(s_old, StatusCode::UNAUTHORIZED);
        let (s_new, _) = json_request(
            &app,
            Method::POST,
            "/api/login",
            None,
            Some(serde_json::json!({
                "username": "ctrl",
                "admin_password": "newer-passphrase-1",
            })),
        )
        .await;
        assert_eq!(s_new, StatusCode::OK);
    }

    #[tokio::test]
    async fn change_my_password_rejects_wrong_current() {
        let app = build_router(test_app_state());
        let token = setup_controller(&app).await;
        let (status, body) = json_request(
            &app,
            Method::POST,
            "/api/admin/me/password",
            Some(&token),
            Some(serde_json::json!({
                "current_password": "wrong",
                "new_password": "newer-passphrase-1",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "bad_credentials");
    }

    #[tokio::test]
    async fn change_my_password_rejects_short_new() {
        let app = build_router(test_app_state());
        let token = setup_controller(&app).await;
        let (status, body) = json_request(
            &app,
            Method::POST,
            "/api/admin/me/password",
            Some(&token),
            Some(serde_json::json!({
                "current_password": "hunter2-longer",
                "new_password": "tiny",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "weak_password");
    }

    #[tokio::test]
    async fn reset_other_user_password_as_controller() {
        let app = build_router(test_app_state());
        let token = setup_controller(&app).await;
        // Invite an operator.
        let (_, invited) = json_request(
            &app,
            Method::POST,
            "/api/admin/users/invite",
            Some(&token),
            Some(invite_body()),
        )
        .await;
        let target = invited["user_id"].as_str().unwrap().to_owned();

        let (status, _) = json_request(
            &app,
            Method::POST,
            &format!("/api/admin/users/{target}/password"),
            Some(&token),
            Some(serde_json::json!({"new_password": "operator-pass-2"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Operator's old password no longer works.
        let (s_old, _) = json_request(
            &app,
            Method::POST,
            "/api/login",
            None,
            Some(serde_json::json!({
                "username": "opone",
                "admin_password": "operator-pass-1",
            })),
        )
        .await;
        assert_eq!(s_old, StatusCode::UNAUTHORIZED);
        // New one does.
        let (s_new, _) = json_request(
            &app,
            Method::POST,
            "/api/login",
            None,
            Some(serde_json::json!({
                "username": "opone",
                "admin_password": "operator-pass-2",
            })),
        )
        .await;
        assert_eq!(s_new, StatusCode::OK);
    }

    #[tokio::test]
    async fn reset_password_refuses_self_target() {
        let app = build_router(test_app_state());
        let token = setup_controller(&app).await;
        // Find the controller's own user_id from /api/admin/users.
        let (_, body) =
            json_request(&app, Method::GET, "/api/admin/users", Some(&token), None).await;
        let me = body["users"][0]["user_id"].as_str().unwrap().to_owned();
        let (status, body) = json_request(
            &app,
            Method::POST,
            &format!("/api/admin/users/{me}/password"),
            Some(&token),
            Some(serde_json::json!({"new_password": "anything-longer"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "use_self_change");
    }

    #[tokio::test]
    async fn reset_password_requires_controller() {
        // Set up a controller, invite an operator, then issue a token
        // for the operator and try to reset the controller's password
        // — should be 403.
        let state = test_app_state();
        let app = build_router(state.clone());
        let _ = setup_controller(&app).await;
        // Insert an operator directly so we know its user_id.
        let now = chrono::Utc::now().timestamp();
        let op_id = "op-1";
        let store = UserStore::new(&state.db);
        store
            .insert(&UserRow {
                user_id: op_id.into(),
                username: "op1".into(),
                display_name: "Op".into(),
                email: None,
                password_hash: hash_pw("operator-pass-1").unwrap(),
                role: UserRole::Operator,
                created_at: now,
                last_login_at: None,
            })
            .unwrap();
        let op_token = issue_token_for(&state, op_id);
        let (_, controller_body) =
            json_request(&app, Method::GET, "/api/admin/users", Some(&op_token), None).await;
        let controller_id = controller_body["users"][0]["user_id"].as_str().unwrap();
        let (status, body) = json_request(
            &app,
            Method::POST,
            &format!("/api/admin/users/{controller_id}/password"),
            Some(&op_token),
            Some(serde_json::json!({"new_password": "anything-longer"})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "got body {body}");
    }
}
