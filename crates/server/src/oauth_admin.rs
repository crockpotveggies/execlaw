//! Admin + callback endpoints for the generic OAuth flow.
//!
//! Routes:
//!   * `GET    /api/admin/oauth/clients`                                 — Controller — list configured (plugin_id, account_name) clients with token-status
//!   * `GET    /api/admin/oauth/clients/{plugin_id}/{account_name}`      — Controller — fetch one
//!   * `PUT    /api/admin/oauth/clients/{plugin_id}/{account_name}`      — Controller — upsert client_id / client_secret / redirect_uri / scopes
//!   * `DELETE /api/admin/oauth/clients/{plugin_id}/{account_name}`      — Controller — wipe client (cascades tokens via FK)
//!   * `POST   /api/admin/oauth/clients/{plugin_id}/{account_name}/connect` — Controller — mints CSRF state, returns authorize URL the SPA opens in a new tab
//!   * `POST   /api/admin/oauth/clients/{plugin_id}/{account_name}/disconnect` — Controller — drops tokens but keeps client config
//!   * `GET    /api/oauth/{provider}/callback?code=&state=`              — UNAUTHENTICATED (CSRF state is the auth) — receives the redirect, exchanges code, stores tokens, returns a tiny HTML "you can close this tab" page
//!
//! The callback is intentionally outside `/api/admin/` because the
//! browser arriving here is following a Google redirect and has no
//! JWT; the random state_token is the binding to the operator's
//! original "Connect Account" click.

use crate::auth_extract::AuthedUser;
use crate::oauth_provider::{
    AuthorizeParams, ExchangeParams, GoogleOauthProvider, OauthProvider, OauthProviderError,
};
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post, put};
use base64::Engine;
use execlaw_core::oauth::{
    OauthClient, OauthClientStore, OauthError, OauthPending, OauthPendingStore, OauthTokenStore,
    OauthTokens,
};
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// CSRF state TTL. Authorize-redirect typically completes in a few
/// seconds; 10 minutes is more than enough headroom while still
/// short enough that a leaked URL stops working quickly.
const PENDING_TTL_SECS: i64 = 600;

impl From<OauthError> for ApiError {
    fn from(e: OauthError) -> Self {
        match e {
            OauthError::NoClient { .. } | OauthError::NoTokens { .. } => ApiError {
                status: StatusCode::NOT_FOUND,
                code: "oauth_not_found",
                message: e.to_string(),
            },
            OauthError::NoPending => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "oauth_state_invalid",
                message: e.to_string(),
            },
            OauthError::Db(db) => db.into(),
        }
    }
}

impl From<OauthProviderError> for ApiError {
    fn from(e: OauthProviderError) -> Self {
        ApiError {
            status: StatusCode::BAD_GATEWAY,
            code: "oauth_provider_error",
            message: e.to_string(),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OauthClientView {
    pub plugin_id: String,
    pub account_name: String,
    pub provider: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// True when tokens are present + non-expired (the SPA shows a
    /// "Connected as" badge); false when the operator hasn't
    /// completed the OAuth dance yet, or when tokens were dropped.
    pub connected: bool,
    /// Email associated with the connected account, when known.
    pub account_email: Option<String>,
    /// Unix-seconds expiry of the current access_token, when present.
    /// SPA renders a "refreshes in N min" line.
    pub token_expires_at: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OauthClientsResponse {
    pub clients: Vec<OauthClientView>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpsertClientRequest {
    pub provider: String,
    pub client_id: String,
    /// Empty string is a sentinel for "leave the persisted secret
    /// alone" — lets the SPA show the form pre-populated without
    /// the operator having to re-paste the secret on every save.
    pub client_secret: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConnectResponse {
    /// SPA opens this in a new tab. Google redirects back to the
    /// configured `redirect_uri` with `code=` and `state=`.
    pub authorize_url: String,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers — Controller-only.

#[utoipa::path(
    get,
    path = "/api/admin/oauth/clients",
    responses(
        (status = 200, description = "Every configured OAuth client + connection status", body = OauthClientsResponse),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "oauth"
)]
pub async fn list_clients_handler(
    State(state): State<AppState>,
    user: AuthedUser,
) -> Result<Json<OauthClientsResponse>, ApiError> {
    require_controller(&state, &user)?;
    let db = state.db.clone();
    let clients =
        tokio::task::spawn_blocking(move || -> Result<Vec<OauthClientView>, OauthError> {
            let cs = OauthClientStore::new(&db);
            let ts = OauthTokenStore::new(&db);
            let rows = list_all_clients(&db)?;
            let mut out = Vec::with_capacity(rows.len());
            for c in rows {
                let toks = ts.get(&c.plugin_id, &c.account_name)?;
                out.push(client_view(&c, toks.as_ref()));
            }
            let _ = cs;
            Ok(out)
        })
        .await
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "join",
            message: e.to_string(),
        })??;
    Ok(Json(OauthClientsResponse { clients }))
}

#[utoipa::path(
    get,
    path = "/api/admin/oauth/clients/{plugin_id}/{account_name}",
    responses(
        (status = 200, description = "One configured client + token status", body = OauthClientView),
        (status = 404, description = "Client not configured"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "oauth"
)]
pub async fn get_client_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((plugin_id, account_name)): Path<(String, String)>,
) -> Result<Json<OauthClientView>, ApiError> {
    require_controller(&state, &user)?;
    let cs = OauthClientStore::new(&state.db);
    let client = cs.get(&plugin_id, &account_name)?.ok_or_else(|| ApiError {
        status: StatusCode::NOT_FOUND,
        code: "oauth_not_found",
        message: format!("no oauth client for {plugin_id}/{account_name}"),
    })?;
    let ts = OauthTokenStore::new(&state.db);
    let tokens = ts.get(&plugin_id, &account_name)?;
    Ok(Json(client_view(&client, tokens.as_ref())))
}

#[utoipa::path(
    put,
    path = "/api/admin/oauth/clients/{plugin_id}/{account_name}",
    request_body = UpsertClientRequest,
    responses(
        (status = 200, description = "Client config saved", body = OauthClientView),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "oauth"
)]
pub async fn upsert_client_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((plugin_id, account_name)): Path<(String, String)>,
    Json(req): Json<UpsertClientRequest>,
) -> Result<Json<OauthClientView>, ApiError> {
    require_controller(&state, &user)?;
    let cs = OauthClientStore::new(&state.db);
    let now = chrono::Utc::now().timestamp();
    let existing = cs.get(&plugin_id, &account_name)?;
    // Empty-string sentinel: keep the persisted secret. Lets the
    // settings page pre-populate the form without re-prompting for
    // a secret on every save.
    let client_secret = if req.client_secret.is_empty() {
        existing
            .as_ref()
            .map(|e| e.client_secret.clone())
            .unwrap_or_default()
    } else {
        req.client_secret
    };
    let scopes_json = serde_json::to_string(&req.scopes).unwrap_or_else(|_| "[]".into());
    let row = OauthClient {
        plugin_id: plugin_id.clone(),
        account_name: account_name.clone(),
        provider: req.provider,
        client_id: req.client_id,
        client_secret,
        redirect_uri: req.redirect_uri,
        scopes_json,
        created_at: existing.as_ref().map(|e| e.created_at).unwrap_or(now),
        updated_at: now,
    };
    cs.upsert(&row)?;
    let ts = OauthTokenStore::new(&state.db);
    let tokens = ts.get(&plugin_id, &account_name)?;
    Ok(Json(client_view(&row, tokens.as_ref())))
}

#[utoipa::path(
    delete,
    path = "/api/admin/oauth/clients/{plugin_id}/{account_name}",
    responses(
        (status = 204, description = "Client removed (tokens cascaded)"),
        (status = 404, description = "Client not configured"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "oauth"
)]
pub async fn delete_client_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((plugin_id, account_name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let cs = OauthClientStore::new(&state.db);
    let removed = cs.delete(&plugin_id, &account_name)?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "oauth_not_found",
            message: format!("no oauth client for {plugin_id}/{account_name}"),
        })
    }
}

#[utoipa::path(
    post,
    path = "/api/admin/oauth/clients/{plugin_id}/{account_name}/connect",
    responses(
        (status = 200, description = "Authorize URL the SPA opens in a new tab", body = ConnectResponse),
        (status = 404, description = "Client not configured"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "oauth"
)]
pub async fn connect_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((plugin_id, account_name)): Path<(String, String)>,
) -> Result<Json<ConnectResponse>, ApiError> {
    require_controller(&state, &user)?;
    let cs = OauthClientStore::new(&state.db);
    let mut client = cs.get(&plugin_id, &account_name)?.ok_or_else(|| ApiError {
        status: StatusCode::NOT_FOUND,
        code: "oauth_not_found",
        message: format!("no oauth client for {plugin_id}/{account_name}"),
    })?;

    // Manifest-driven scope sync. Without this, a plugin upgrade
    // that adds new scopes (google-calendar v0.1 read-only → v0.2
    // with `calendar.events`) leaves the persisted client row stuck
    // at the old narrower set. The next connect builds an authorize
    // URL with stale scopes, Google issues a token without the new
    // permissions, and write tools 401 forever — the symptom that
    // prompted this fix. Reconciling here makes the manifest
    // (which the operator can audit on disk before installing the
    // plugin) the source of truth for what scopes get requested.
    //
    // Take the union: persisted ∪ manifest. Operators who deliberately
    // ADDED a scope (rare; today the SPA always pushes the manifest
    // list) keep their addition. The common case — manifest is a
    // strict superset — collapses to "use manifest scopes".
    {
        let accounts = state.plugin_host.registry().oauth_accounts_for(&plugin_id);
        if let Some(decl) = accounts.into_iter().find(|a| a.account_name == account_name) {
            if !decl.scopes.is_empty() {
                let persisted = client.scopes();
                let mut merged: Vec<String> = persisted.clone();
                let mut changed = false;
                for s in &decl.scopes {
                    if !merged.iter().any(|p| p == s) {
                        merged.push(s.clone());
                        changed = true;
                    }
                }
                if changed {
                    let json = serde_json::to_string(&merged).unwrap_or_else(|_| "[]".into());
                    client.scopes_json = json;
                    let _ = cs.upsert(&client);
                    tracing::info!(
                        target: "oauth_admin::connect",
                        plugin_id = %plugin_id,
                        account_name = %account_name,
                        added = ?decl
                            .scopes
                            .iter()
                            .filter(|s| !persisted.iter().any(|p| p == *s))
                            .collect::<Vec<_>>(),
                        "synced persisted oauth scopes from manifest before authorize",
                    );
                }
            }
        }
    }

    // Random state token — base64-url(32 bytes) so it's URL-safe
    // without escaping.
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let state_token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let now = chrono::Utc::now().timestamp();
    OauthPendingStore::new(&state.db).insert(&OauthPending {
        state_token: state_token.clone(),
        plugin_id: plugin_id.clone(),
        account_name: account_name.clone(),
        redirect_to: None,
        created_at: now,
        expires_at: now + PENDING_TTL_SECS,
    })?;
    let provider = pick_provider(&client.provider)?;
    let url = provider.build_authorize_url(&AuthorizeParams {
        client_id: client.client_id.clone(),
        redirect_uri: client.redirect_uri.clone(),
        scopes: client.scopes(),
        state_token,
    })?;
    Ok(Json(ConnectResponse { authorize_url: url }))
}

#[utoipa::path(
    post,
    path = "/api/admin/oauth/clients/{plugin_id}/{account_name}/disconnect",
    responses(
        (status = 204, description = "Tokens removed; client config retained"),
        (status = 404, description = "No tokens to remove"),
        (status = 403, description = "Caller is not a Controller"),
    ),
    security(("bearer_jwt" = [])),
    tag = "oauth"
)]
pub async fn disconnect_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path((plugin_id, account_name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    require_controller(&state, &user)?;
    let ts = OauthTokenStore::new(&state.db);
    let removed = ts.delete(&plugin_id, &account_name)?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "oauth_no_tokens",
            message: format!("no oauth tokens for {plugin_id}/{account_name}"),
        })
    }
}

/// UNAUTHENTICATED. The state_token (CSRF) is the auth — without a
/// matching pending row, the request is rejected. Lands at this URL
/// because the operator configured `redirect_uri` to point here.
pub async fn callback_handler(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let html = match callback_inner(&state, &provider_id, q).await {
        Ok(success_html) => success_html,
        Err(e) => render_error_html(&e.message),
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

async fn callback_inner(
    state: &AppState,
    provider_id: &str,
    q: CallbackQuery,
) -> Result<String, ApiError> {
    if let Some(err) = q.error.as_deref() {
        let detail = q.error_description.as_deref().unwrap_or("");
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "oauth_provider_denied",
            message: format!("provider denied: {err} {detail}"),
        });
    }
    let code = q.code.ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "oauth_callback_missing_code",
        message: "callback missing 'code' query param".into(),
    })?;
    let state_token = q.state.ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "oauth_callback_missing_state",
        message: "callback missing 'state' query param".into(),
    })?;
    let now = chrono::Utc::now().timestamp();
    let pending = OauthPendingStore::new(&state.db).consume(&state_token, now)?;
    let cs = OauthClientStore::new(&state.db);
    let client = cs
        .get(&pending.plugin_id, &pending.account_name)?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "oauth_not_found",
            message: "client config disappeared between connect and callback".into(),
        })?;
    if client.provider != provider_id {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "oauth_provider_mismatch",
            message: format!(
                "callback provider '{provider_id}' doesn't match client '{}'",
                client.provider
            ),
        });
    }
    let provider = pick_provider(&client.provider)?;
    let grant = provider
        .exchange_code(&ExchangeParams {
            client_id: client.client_id.clone(),
            client_secret: client.client_secret.clone(),
            redirect_uri: client.redirect_uri.clone(),
            code,
        })
        .await?;
    // Best-effort userinfo fetch; failure here is non-fatal — the
    // tokens are already valid, the email is just for display.
    let email = provider
        .fetch_userinfo(&grant.access_token)
        .await
        .ok()
        .and_then(|u| u.email);
    let scopes_granted = grant
        .scope
        .clone()
        .map(|s| {
            serde_json::to_string(&s.split_whitespace().collect::<Vec<_>>())
                .unwrap_or_else(|_| "[]".into())
        })
        .unwrap_or_else(|| client.scopes_json.clone());
    OauthTokenStore::new(&state.db).upsert(&OauthTokens {
        plugin_id: pending.plugin_id.clone(),
        account_name: pending.account_name.clone(),
        access_token: grant.access_token,
        refresh_token: grant.refresh_token,
        token_expires_at: now + grant.expires_in_secs,
        scopes_granted,
        account_email: email.clone(),
        created_at: now,
        updated_at: now,
    })?;
    Ok(render_success_html(
        &pending.plugin_id,
        &pending.account_name,
        email.as_deref(),
    ))
}

// ---------------------------------------------------------------------------
// Helpers.

fn list_all_clients(db: &execlaw_core::Database) -> Result<Vec<OauthClient>, OauthError> {
    let rows = db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT plugin_id, account_name, provider, client_id, client_secret, redirect_uri, scopes_json, created_at, updated_at \
                 FROM state_oauth_clients ORDER BY plugin_id ASC, account_name ASC",
            )?;
            let rows: Vec<OauthClient> = stmt
                .query_map([], |row| {
                    Ok(OauthClient {
                        plugin_id: row.get(0)?,
                        account_name: row.get(1)?,
                        provider: row.get(2)?,
                        client_id: row.get(3)?,
                        client_secret: row.get(4)?,
                        redirect_uri: row.get(5)?,
                        scopes_json: row.get(6)?,
                        created_at: row.get(7)?,
                        updated_at: row.get(8)?,
                    })
                })?
                .filter_map(Result::ok)
                .collect();
            Ok(rows)
        })
        .map_err(OauthError::Db)?;
    Ok(rows)
}

fn client_view(c: &OauthClient, tokens: Option<&OauthTokens>) -> OauthClientView {
    OauthClientView {
        plugin_id: c.plugin_id.clone(),
        account_name: c.account_name.clone(),
        provider: c.provider.clone(),
        client_id: c.client_id.clone(),
        redirect_uri: c.redirect_uri.clone(),
        scopes: c.scopes(),
        created_at: c.created_at,
        updated_at: c.updated_at,
        connected: tokens.is_some(),
        account_email: tokens.and_then(|t| t.account_email.clone()),
        token_expires_at: tokens.map(|t| t.token_expires_at),
    }
}

fn pick_provider(provider_id: &str) -> Result<Box<dyn OauthProvider>, ApiError> {
    match provider_id {
        "google" => Ok(Box::new(GoogleOauthProvider::default())),
        other => Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "oauth_provider_unknown",
            message: format!("unknown oauth provider '{other}'"),
        }),
    }
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
            message: "only a Controller can manage OAuth clients".into(),
        }),
    }
}

fn render_success_html(plugin_id: &str, account_name: &str, email: Option<&str>) -> String {
    let who = email.unwrap_or("(account email unavailable)");
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Connected</title>
<style>body{{font-family:system-ui,sans-serif;background:#0d1117;color:#c9d1d9;
padding:3rem;max-width:42rem;margin:auto;line-height:1.5}}
h1{{color:#7ee787}} code{{background:#161b22;padding:.1rem .4rem;border-radius:.3rem}}
button{{background:#238636;color:#fff;border:0;padding:.5rem 1rem;border-radius:.3rem;
font-size:1rem;cursor:pointer}}</style></head><body>
<h1>✓ Connected</h1>
<p>Plugin <code>{plugin_id}</code> account <code>{account_name}</code> is now connected as <strong>{who}</strong>.</p>
<p>You can close this tab and return to the settings page.</p>
<button onclick="window.close()">Close tab</button>
</body></html>"#,
    )
}

fn render_error_html(message: &str) -> String {
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Connection failed</title>
<style>body{{font-family:system-ui,sans-serif;background:#0d1117;color:#c9d1d9;
padding:3rem;max-width:42rem;margin:auto;line-height:1.5}}
h1{{color:#ff7b72}} pre{{background:#161b22;padding:1rem;border-radius:.3rem;
overflow-x:auto;white-space:pre-wrap}}</style></head><body>
<h1>✗ Connection failed</h1>
<pre>{message}</pre>
<p>Close this tab and try again from the settings page.</p>
</body></html>"#,
        message = html_escape(message),
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn oauth_admin_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/oauth/clients", get(list_clients_handler))
        .route(
            "/api/admin/oauth/clients/{plugin_id}/{account_name}",
            get(get_client_handler),
        )
        .route(
            "/api/admin/oauth/clients/{plugin_id}/{account_name}",
            put(upsert_client_handler),
        )
        .route(
            "/api/admin/oauth/clients/{plugin_id}/{account_name}",
            delete(delete_client_handler),
        )
        .route(
            "/api/admin/oauth/clients/{plugin_id}/{account_name}/connect",
            post(connect_handler),
        )
        .route(
            "/api/admin/oauth/clients/{plugin_id}/{account_name}/disconnect",
            post(disconnect_handler),
        )
        .route("/api/oauth/{provider}/callback", get(callback_handler))
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
    async fn upsert_then_get_round_trips_client_config() {
        let state = test_app_state();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let body = serde_json::json!({
            "provider": "google",
            "client_id": "abc.apps.googleusercontent.com",
            "client_secret": "GOCSPX-secret",
            "redirect_uri": "http://localhost:3030/api/oauth/google/callback",
            "scopes": ["https://www.googleapis.com/auth/contacts.readonly"],
        })
        .to_string();
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/oauth/clients/plugin-google-contacts/controller")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Get it back.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/oauth/clients/plugin-google-contacts/controller")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["client_id"], "abc.apps.googleusercontent.com");
        assert_eq!(v["connected"], false);
        assert!(v["scopes"].is_array());
    }

    #[tokio::test]
    async fn empty_client_secret_preserves_existing_value() {
        // SPA pre-populates the form; saving without re-pasting the
        // secret must not nuke it. The empty-string sentinel is the
        // contract.
        let state = test_app_state();
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;
        // First save with a real secret.
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/oauth/clients/p/a")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "provider": "google",
                    "client_id": "cid",
                    "client_secret": "REAL",
                    "redirect_uri": "http://x",
                    "scopes": ["s"],
                })
                .to_string(),
            ))
            .unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();
        // Second save: secret = "" sentinel.
        let req = Request::builder()
            .method(Method::PUT)
            .uri("/api/admin/oauth/clients/p/a")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "provider": "google",
                    "client_id": "cid-rotated",
                    "client_secret": "",
                    "redirect_uri": "http://y",
                    "scopes": ["s"],
                })
                .to_string(),
            ))
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
        // Direct DB read confirms the persisted secret survived.
        let cs = OauthClientStore::new(&state.db);
        let c = cs.get("p", "a").unwrap().unwrap();
        assert_eq!(c.client_secret, "REAL");
        assert_eq!(c.client_id, "cid-rotated");
        assert_eq!(c.redirect_uri, "http://y");
    }

    #[tokio::test]
    async fn connect_returns_authorize_url_after_client_configured() {
        let state = test_app_state();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let body = serde_json::json!({
            "provider": "google",
            "client_id": "cid",
            "client_secret": "secret",
            "redirect_uri": "http://localhost:3030/api/oauth/google/callback",
            "scopes": ["https://www.googleapis.com/auth/contacts.readonly"],
        });
        app.clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/admin/oauth/clients/plugin-google-contacts/controller")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/oauth/clients/plugin-google-contacts/controller/connect")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let url = v["authorize_url"].as_str().unwrap();
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains("state=")); // CSRF token present
        assert!(url.contains("client_id=cid"));
    }

    #[tokio::test]
    async fn connect_syncs_persisted_scopes_from_manifest_on_drift() {
        // Pin the scope-drift fix: an operator who connected the
        // plugin under v0.1 (read-only) and then upgraded to v0.2
        // (which added a write scope) had stale persisted scopes
        // that locked the next OAuth grant into the old narrow set.
        // Connect now reconciles persisted ∪ manifest BEFORE building
        // the authorize URL so the next consent prompt requests the
        // expanded scopes.
        use execlaw_plugin_sdk::manifest::{
            OauthAccountDecl, PluginHeader, PluginManifest,
        };

        let state = test_app_state();

        // Register a plugin manifest with a richer scope set than
        // what the operator's persisted client carries. The cached
        // manifest scopes are what `connect_handler` should sync
        // into `state_oauth_clients.scopes_json`.
        let manifest = PluginManifest {
            plugin: PluginHeader {
                id: "plugin-google-calendar".into(),
                name: "Google Calendar".into(),
                version: "0.2.1".into(),
                description: None,
                author: None,
                homepage: None,
                license: None,
                core_version: None,
            },
            tools: vec![],
            transport: None,
            identity_provider: None,
            inference_backend: None,
            hardware_probe: None,
            ui_panels: vec![],
            chat_components: vec![],
            event_subscriptions: vec![],
            services: vec![],
            oauth_accounts: vec![OauthAccountDecl {
                name: "controller".into(),
                provider: "google".into(),
                scopes: vec![
                    "https://www.googleapis.com/auth/calendar.readonly".into(),
                    "https://www.googleapis.com/auth/calendar.events".into(),
                    "openid".into(),
                    "email".into(),
                ],
                proactive_refresh_window: None,
                warn_before_expiry: vec![],
                token_store: None,
            }],
            alert_sources: vec![],
            health_checks: vec![],
            skills: vec![],
            admin_routes: vec![],
            runtime: None,
        };
        state
            .plugin_host
            .registry()
            .enable_with_stage(&manifest, None)
            .expect("register manifest");

        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;

        // Persist the v0.1 client config — read-only only.
        let body = serde_json::json!({
            "provider": "google",
            "client_id": "cid",
            "client_secret": "secret",
            "redirect_uri": "http://localhost:3030/api/oauth/google/callback",
            "scopes": ["https://www.googleapis.com/auth/calendar.readonly"],
        });
        app.clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/admin/oauth/clients/plugin-google-calendar/controller")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Connect — handler should silently expand the persisted
        // scopes BEFORE building the authorize URL.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/admin/oauth/clients/plugin-google-calendar/controller/connect")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let url = v["authorize_url"].as_str().unwrap();
        // The URL-encoded scope param must contain the new write
        // scope (`%2F` is the encoded `/`). Without the sync, this
        // assertion fails — the regression test that pins the fix.
        assert!(
            url.contains("calendar.events"),
            "authorize_url must include the manifest's write scope; got: {url}"
        );
        assert!(url.contains("calendar.readonly"));

        // The persisted row must now reflect the union — so the
        // SPA's next GET /clients reflects the live scope set.
        let cs = OauthClientStore::new(&state.db);
        let row = cs
            .get("plugin-google-calendar", "controller")
            .unwrap()
            .expect("client row must exist after upsert");
        let scopes = row.scopes();
        assert!(scopes.iter().any(|s| s == "https://www.googleapis.com/auth/calendar.events"));
        assert!(scopes.iter().any(|s| s == "https://www.googleapis.com/auth/calendar.readonly"));
        assert!(scopes.iter().any(|s| s == "openid"));
        assert!(scopes.iter().any(|s| s == "email"));
    }

    #[tokio::test]
    async fn callback_rejects_request_with_bad_state_token() {
        let state = test_app_state();
        let app = build_router(state);
        // No pending row → state is rejected. Endpoint is
        // unauthenticated by design (the state IS the auth).
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/oauth/google/callback?code=x&state=does-not-exist")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Returns 200 with error HTML rather than 4xx — operator
        // sees the failure in the popup tab, no JSON for the
        // browser to parse.
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let html = std::str::from_utf8(&bytes).unwrap();
        assert!(html.contains("Connection failed"));
        assert!(html.contains("oauth state token not found"));
    }

    #[tokio::test]
    async fn callback_rejects_provider_error_query_params() {
        // Google bounces the user back with ?error=access_denied
        // when they decline at the consent screen.
        let state = test_app_state();
        let app = build_router(state);
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/oauth/google/callback?error=access_denied&error_description=user+declined")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let html = std::str::from_utf8(&bytes).unwrap();
        assert!(html.contains("access_denied"));
    }

    #[tokio::test]
    async fn list_clients_rejects_non_controller() {
        // Anonymous request → 401 (no token); we expect the auth
        // layer to gate before the handler runs.
        let state = test_app_state();
        let app = build_router(state);
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/oauth/clients")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(
            resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        );
    }

    #[tokio::test]
    async fn disconnect_drops_tokens_but_keeps_client() {
        let state = test_app_state();
        // Manually seed a client + tokens via the stores so we can
        // skip the OAuth flow.
        let now = chrono::Utc::now().timestamp();
        OauthClientStore::new(&state.db)
            .upsert(&OauthClient {
                plugin_id: "p".into(),
                account_name: "a".into(),
                provider: "google".into(),
                client_id: "cid".into(),
                client_secret: "s".into(),
                redirect_uri: "http://x".into(),
                scopes_json: "[]".into(),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        OauthTokenStore::new(&state.db)
            .upsert(&OauthTokens {
                plugin_id: "p".into(),
                account_name: "a".into(),
                access_token: "ya29".into(),
                refresh_token: Some("rt".into()),
                token_expires_at: now + 3600,
                scopes_granted: "[]".into(),
                account_email: Some("u@x".into()),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let app = build_router(state.clone());
        let tok = setup_controller_token(&app).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/oauth/clients/p/a/disconnect")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        // Client config is still there; tokens are gone.
        assert!(
            OauthClientStore::new(&state.db)
                .get("p", "a")
                .unwrap()
                .is_some()
        );
        assert!(
            OauthTokenStore::new(&state.db)
                .get("p", "a")
                .unwrap()
                .is_none()
        );
    }
}
