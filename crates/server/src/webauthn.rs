//! WebAuthn second-factor service + HTTP routes (Phase 7e).
//!
//! Lifecycle:
//!   1. Operator signs in with username + password (existing /api/login).
//!   2. If they have ANY rows in `state_webauthn_credentials`, /api/login
//!      returns 200 + a `ceremony_id` + `RequestChallengeResponse` instead
//!      of issuing tokens. The login route stashes a `PendingAuth`
//!      record under the ceremony id.
//!   3. SPA invokes `navigator.credentials.get(options)` and posts the
//!      assertion to /api/login/webauthn/finish with the ceremony id.
//!   4. Server consumes the ceremony, calls
//!      `Webauthn::finish_passkey_authentication`, bumps the credential's
//!      counter, and issues access + refresh tokens.
//!
//! Registration is the same dance under /api/admin/webauthn/register.
//! Auth is required on the registration routes — operators only register
//! credentials AFTER they've successfully signed in once.
//!
//! Challenge state is held in two `DashMap`s with a 5-minute TTL. Failed
//! ceremonies are eventually GC'd by `prune_expired`; the time check on
//! consume is the security-critical path.
//!
//! ## Build feature gate
//!
//! The relying-party crate `webauthn-rs` pulls in `openssl-sys`, which
//! cannot build on a stock Windows-host dev environment without
//! Strawberry Perl. To keep the workspace `cargo build` green on every
//! host, this whole subsystem lives behind the `webauthn` feature
//! (default-on). When the feature is off, the routes return 503
//! `webauthn_unconfigured`, registration is impossible, and
//! `count_for_user` always returns 0 so the login-route branch never
//! fires.

use crate::auth::{JwtSigner, RefreshStore};
use crate::routes::ApiError;
use crate::state::{AppState, ServerConfig};
use axum::http::StatusCode;
use axum::response::Json;
use axum::Router;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Common error type — defined for both feature paths so callers don't need
// `cfg` themselves.
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum WebauthnSvcError {
    #[error("webauthn relying-party build failed: {0}")]
    Build(String),
    #[error("webauthn ceremony failed: {0}")]
    Ceremony(String),
    #[error("ceremony id not found")]
    UnknownCeremony,
    #[error("ceremony expired")]
    ExpiredCeremony,
    #[error("webauthn feature not compiled in")]
    FeatureDisabled,
}

impl From<WebauthnSvcError> for ApiError {
    fn from(err: WebauthnSvcError) -> Self {
        match err {
            WebauthnSvcError::UnknownCeremony => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "unknown_ceremony",
                message: "no in-flight webauthn ceremony with that id".into(),
            },
            WebauthnSvcError::ExpiredCeremony => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "ceremony_expired",
                message: "webauthn ceremony has expired; restart the flow".into(),
            },
            WebauthnSvcError::Ceremony(msg) => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "webauthn_failed",
                message: msg,
            },
            WebauthnSvcError::Build(msg) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "webauthn_unconfigured",
                message: msg,
            },
            WebauthnSvcError::FeatureDisabled => ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "webauthn_unconfigured",
                message: "the server was built without webauthn support".into(),
            },
        }
    }
}

pub type SharedWebauthn = Arc<WebauthnSvc>;

// ---------------------------------------------------------------------------
// HTTP request / response shapes — used by both feature paths so the
// utoipa schema is stable regardless of build config.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct RegisterBeginRequest {
    /// Operator-supplied label so they can tell their devices apart
    /// in the credential list ("YubiKey 5C", "MacBook TouchID").
    #[schema(example = "YubiKey 5C")]
    pub label: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterBeginResponse {
    pub ceremony_id: String,
    /// Opaque CreationChallengeResponse — the SPA hands this directly
    /// to `navigator.credentials.create({ publicKey })`.
    #[schema(value_type = serde_json::Value)]
    pub options: serde_json::Value,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RegisterFinishRequest {
    pub ceremony_id: String,
    /// Output of `navigator.credentials.create()` — passed straight
    /// to `webauthn-rs::finish_passkey_registration`.
    #[schema(value_type = serde_json::Value)]
    pub credential: serde_json::Value,
}

/// Server-side mirror of `execlaw_core::webauthn::WebauthnCredentialSummary`
/// so we can derive `ToSchema` without making the core crate take a
/// utoipa dep.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CredentialView {
    pub credential_id: String,
    pub label: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

impl From<&execlaw_core::webauthn::WebauthnCredentialRow> for CredentialView {
    fn from(row: &execlaw_core::webauthn::WebauthnCredentialRow) -> Self {
        Self {
            credential_id: row.credential_id.clone(),
            label: row.label.clone(),
            created_at: row.created_at,
            last_used_at: row.last_used_at,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CredentialListResponse {
    pub credentials: Vec<CredentialView>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AssertBeginResponse {
    pub webauthn_required: bool, // always true; kept explicit for the SPA's discriminator
    pub ceremony_id: String,
    /// Opaque RequestChallengeResponse — handed to
    /// `navigator.credentials.get({ publicKey })`.
    #[schema(value_type = serde_json::Value)]
    pub options: serde_json::Value,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct LoginAssertFinishRequest {
    pub ceremony_id: String,
    /// Output of `navigator.credentials.get()`.
    #[schema(value_type = serde_json::Value)]
    pub credential: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Mint access + refresh tokens for a user. Shared with the password
// path in routes.rs and the webauthn-finish path here. Bumps the
// `last_login_at` stamp as a side-effect.
// ---------------------------------------------------------------------------

pub fn issue_login_tokens(
    state: &AppState,
    user_id: &str,
) -> Result<Json<crate::routes::LoginResponse>, ApiError> {
    let users = execlaw_core::users::UserStore::new(&state.db);
    let _ = users.touch_login(user_id, Utc::now().timestamp());

    let session_id = Uuid::new_v4().to_string();
    let access = state
        .signer
        .issue_access_token(user_id, &session_id, state.config.access_token_ttl_secs)?;
    let refresh = state.refresh_store.issue(
        user_id,
        &session_id,
        state.config.refresh_token_ttl_secs,
    )?;
    Ok(Json(crate::routes::LoginResponse {
        access_token: access,
        refresh_token: refresh,
    }))
}

// `Signer` + `RefreshStore` are kept public-imported above so the
// helper signatures don't drift if `routes.rs` is reorganised.
#[allow(dead_code)]
fn _signer_marker(_: &Arc<JwtSigner>, _: &Arc<RefreshStore>) {}

#[allow(dead_code)]
fn _config_marker(_: &ServerConfig) {}

// ===========================================================================
// REAL implementation — `webauthn` feature on.
// ===========================================================================

#[cfg(feature = "webauthn")]
mod imp {
    use super::*;
    use axum::extract::{Path as AxumPath, State};
    use axum::routing::{delete, get, post};
    use crate::auth_extract::AuthedUser;
    use dashmap::DashMap;
    use execlaw_core::webauthn::{WebauthnCredentialRow, WebauthnStore};
    use webauthn_rs::prelude::{
        CreationChallengeResponse, Passkey, PasskeyAuthentication, PasskeyRegistration,
        PublicKeyCredential, RegisterPublicKeyCredential, RequestChallengeResponse, Url, Webauthn,
        WebauthnBuilder,
    };

    /// How long a begin/finish ceremony stays alive on the server.
    /// 5 minutes matches the WebAuthn `timeout` field's typical default
    /// — give the user enough time to find their authenticator without
    /// keeping stale challenges around indefinitely.
    pub const CEREMONY_TTL_SECS: i64 = 5 * 60;

    /// In-memory record of an in-flight registration ceremony.
    pub(super) struct PendingRegistration {
        pub user_id: String,
        pub label: String,
        pub state: PasskeyRegistration,
        pub expires_at: i64,
    }

    /// In-memory record of an in-flight authentication ceremony.
    pub(super) struct PendingAuth {
        pub user_id: String,
        pub state: PasskeyAuthentication,
        pub expires_at: i64,
    }

    /// WebAuthn service: relying-party config + ceremony state.
    pub struct WebauthnSvc {
        rp: Webauthn,
        pub(super) pending_register: DashMap<String, PendingRegistration>,
        pub(super) pending_auth: DashMap<String, PendingAuth>,
    }

    impl std::fmt::Debug for WebauthnSvc {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("WebauthnSvc")
                .field("pending_register", &self.pending_register.len())
                .field("pending_auth", &self.pending_auth.len())
                .finish()
        }
    }

    impl WebauthnSvc {
        pub fn new(rp_id: &str, rp_origin: &str, rp_name: &str) -> Result<Self, WebauthnSvcError> {
            let url =
                Url::parse(rp_origin).map_err(|e| WebauthnSvcError::Build(e.to_string()))?;
            let rp = WebauthnBuilder::new(rp_id, &url)
                .map_err(|e| WebauthnSvcError::Build(e.to_string()))?
                .rp_name(rp_name)
                .build()
                .map_err(|e| WebauthnSvcError::Build(e.to_string()))?;
            Ok(Self {
                rp,
                pending_register: DashMap::new(),
                pending_auth: DashMap::new(),
            })
        }

        pub fn begin_registration(
            &self,
            user_id: &str,
            username: &str,
            display_name: &str,
            label: String,
            existing_creds: Vec<Passkey>,
        ) -> Result<(String, CreationChallengeResponse), WebauthnSvcError> {
            let handle = uuid_from_user_id(user_id);
            let exclude: Vec<webauthn_rs::prelude::CredentialID> =
                existing_creds.iter().map(|p| p.cred_id().clone()).collect();
            let (ccr, state) = self
                .rp
                .start_passkey_registration(
                    handle,
                    username,
                    display_name,
                    if exclude.is_empty() { None } else { Some(exclude) },
                )
                .map_err(|e| WebauthnSvcError::Ceremony(e.to_string()))?;
            let ceremony_id = Uuid::new_v4().to_string();
            self.pending_register.insert(
                ceremony_id.clone(),
                PendingRegistration {
                    user_id: user_id.to_owned(),
                    label,
                    state,
                    expires_at: Utc::now().timestamp() + CEREMONY_TTL_SECS,
                },
            );
            Ok((ceremony_id, ccr))
        }

        pub fn finish_registration(
            &self,
            ceremony_id: &str,
            cred: &RegisterPublicKeyCredential,
        ) -> Result<(String, String, Passkey), WebauthnSvcError> {
            let entry = self
                .pending_register
                .remove(ceremony_id)
                .ok_or(WebauthnSvcError::UnknownCeremony)?;
            let (_, pending) = entry;
            if Utc::now().timestamp() > pending.expires_at {
                return Err(WebauthnSvcError::ExpiredCeremony);
            }
            let passkey = self
                .rp
                .finish_passkey_registration(cred, &pending.state)
                .map_err(|e| WebauthnSvcError::Ceremony(e.to_string()))?;
            Ok((pending.user_id, pending.label, passkey))
        }

        pub fn begin_authentication(
            &self,
            user_id: &str,
            creds: &[Passkey],
        ) -> Result<(String, RequestChallengeResponse), WebauthnSvcError> {
            let (rcr, state) = self
                .rp
                .start_passkey_authentication(creds)
                .map_err(|e| WebauthnSvcError::Ceremony(e.to_string()))?;
            let ceremony_id = Uuid::new_v4().to_string();
            self.pending_auth.insert(
                ceremony_id.clone(),
                PendingAuth {
                    user_id: user_id.to_owned(),
                    state,
                    expires_at: Utc::now().timestamp() + CEREMONY_TTL_SECS,
                },
            );
            Ok((ceremony_id, rcr))
        }

        pub fn finish_authentication(
            &self,
            ceremony_id: &str,
            cred: &PublicKeyCredential,
        ) -> Result<(String, webauthn_rs::prelude::CredentialID, u32), WebauthnSvcError> {
            let entry = self
                .pending_auth
                .remove(ceremony_id)
                .ok_or(WebauthnSvcError::UnknownCeremony)?;
            let (_, pending) = entry;
            if Utc::now().timestamp() > pending.expires_at {
                return Err(WebauthnSvcError::ExpiredCeremony);
            }
            let result = self
                .rp
                .finish_passkey_authentication(cred, &pending.state)
                .map_err(|e| WebauthnSvcError::Ceremony(e.to_string()))?;
            Ok((pending.user_id, result.cred_id().clone(), result.counter()))
        }

        pub fn prune_expired(&self) {
            let now = Utc::now().timestamp();
            self.pending_register.retain(|_, v| v.expires_at > now);
            self.pending_auth.retain(|_, v| v.expires_at > now);
        }

        #[cfg(test)]
        pub fn pending_count(&self) -> (usize, usize) {
            (self.pending_register.len(), self.pending_auth.len())
        }
    }

    fn uuid_from_user_id(user_id: &str) -> Uuid {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"execlaw.webauthn.user_handle.v1:");
        h.update(user_id.as_bytes());
        let digest = h.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        Uuid::from_bytes(bytes)
    }

    pub async fn register_begin_handler(
        State(state): State<AppState>,
        user: AuthedUser,
        Json(req): Json<RegisterBeginRequest>,
    ) -> Result<Json<RegisterBeginResponse>, ApiError> {
        let svc = state.webauthn.as_ref().ok_or_else(webauthn_unconfigured)?;
        let label = req.label.trim();
        if label.is_empty() || label.len() > 80 {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "bad_label",
                message: "label must be 1-80 characters".into(),
            });
        }
        let users = execlaw_core::users::UserStore::new(&state.db);
        let row = users
            .get_by_id(&user.user_id)
            .map_err(ApiError::from)?
            .ok_or_else(|| ApiError {
                status: StatusCode::UNAUTHORIZED,
                code: "user_missing",
                message: "authenticated user not found".into(),
            })?;
        let store = WebauthnStore::new(&state.db);
        let existing_rows = store.list_for_user(&user.user_id).map_err(ApiError::from)?;
        let existing_passkeys: Vec<Passkey> = existing_rows
            .iter()
            .filter_map(|r| serde_json::from_str::<Passkey>(&r.passkey_json).ok())
            .collect();

        let (ceremony_id, ccr) = svc
            .begin_registration(
                &user.user_id,
                &row.username,
                &row.display_name,
                label.to_owned(),
                existing_passkeys,
            )
            .map_err(ApiError::from)?;
        let options = serde_json::to_value(&ccr).map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "ceremony_serialise",
            message: e.to_string(),
        })?;
        Ok(Json(RegisterBeginResponse {
            ceremony_id,
            options,
        }))
    }

    pub async fn register_finish_handler(
        State(state): State<AppState>,
        user: AuthedUser,
        Json(req): Json<RegisterFinishRequest>,
    ) -> Result<Json<CredentialView>, ApiError> {
        let svc = state.webauthn.as_ref().ok_or_else(webauthn_unconfigured)?;
        let cred: RegisterPublicKeyCredential =
            serde_json::from_value(req.credential).map_err(|e| ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "bad_credential",
                message: e.to_string(),
            })?;

        let (ceremony_user, label, passkey) = svc
            .finish_registration(&req.ceremony_id, &cred)
            .map_err(ApiError::from)?;
        if ceremony_user != user.user_id {
            return Err(ApiError {
                status: StatusCode::FORBIDDEN,
                code: "ceremony_user_mismatch",
                message: "ceremony was not started by this user".into(),
            });
        }

        let credential_id = base64_url_encode(passkey.cred_id().as_ref());
        let passkey_json = serde_json::to_string(&passkey).map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "passkey_serialise",
            message: e.to_string(),
        })?;
        let now = Utc::now().timestamp();
        let row = WebauthnCredentialRow {
            credential_id: credential_id.clone(),
            user_id: user.user_id.clone(),
            label: label.clone(),
            passkey_json,
            counter: passkey.counter() as i64,
            created_at: now,
            last_used_at: None,
        };
        WebauthnStore::new(&state.db)
            .insert(&row)
            .map_err(ApiError::from)?;

        let audit = execlaw_core::audit::AuditStore::new(&state.db);
        let _ = audit.insert(
            &user.user_id,
            "state_webauthn_credentials",
            &credential_id,
            None,
            Some(&serde_json::json!({"label": label, "user_id": user.user_id})),
        );
        Ok(Json((&row).into()))
    }

    pub async fn list_credentials_handler(
        State(state): State<AppState>,
        user: AuthedUser,
    ) -> Result<Json<CredentialListResponse>, ApiError> {
        let store = WebauthnStore::new(&state.db);
        let rows = store.list_for_user(&user.user_id).map_err(ApiError::from)?;
        let credentials = rows.iter().map(CredentialView::from).collect();
        Ok(Json(CredentialListResponse { credentials }))
    }

    pub async fn delete_credential_handler(
        State(state): State<AppState>,
        user: AuthedUser,
        AxumPath(credential_id): AxumPath<String>,
    ) -> Result<StatusCode, ApiError> {
        let store = WebauthnStore::new(&state.db);
        let removed = store
            .delete_owned(&credential_id, &user.user_id)
            .map_err(ApiError::from)?;
        if !removed {
            return Err(ApiError {
                status: StatusCode::NOT_FOUND,
                code: "credential_not_found",
                message: "no credential with that id owned by the caller".into(),
            });
        }
        let audit = execlaw_core::audit::AuditStore::new(&state.db);
        let _ = audit.insert(
            &user.user_id,
            "state_webauthn_credentials",
            &credential_id,
            Some(&serde_json::json!({"deleted": true})),
            None,
        );
        Ok(StatusCode::OK)
    }

    pub async fn login_assert_finish_handler(
        State(state): State<AppState>,
        Json(req): Json<LoginAssertFinishRequest>,
    ) -> Result<Json<crate::routes::LoginResponse>, ApiError> {
        let svc = state.webauthn.as_ref().ok_or_else(webauthn_unconfigured)?;
        let cred: PublicKeyCredential =
            serde_json::from_value(req.credential).map_err(|e| ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "bad_credential",
                message: e.to_string(),
            })?;
        let (user_id, cred_id, new_counter) = svc
            .finish_authentication(&req.ceremony_id, &cred)
            .map_err(ApiError::from)?;

        let credential_id = base64_url_encode(cred_id.as_ref());
        let store = WebauthnStore::new(&state.db);
        let row = store
            .get(&credential_id)
            .map_err(ApiError::from)?
            .ok_or_else(|| ApiError {
                status: StatusCode::UNAUTHORIZED,
                code: "credential_unknown",
                message: "credential not registered for any user".into(),
            })?;
        if row.user_id != user_id {
            return Err(ApiError {
                status: StatusCode::UNAUTHORIZED,
                code: "credential_user_mismatch",
                message: "credential does not belong to the asserted user".into(),
            });
        }
        if row.counter > 0 && (new_counter as i64) <= row.counter {
            return Err(ApiError {
                status: StatusCode::UNAUTHORIZED,
                code: "counter_replay",
                message: "credential counter regressed; suspected clone".into(),
            });
        }
        let now = Utc::now().timestamp();
        store
            .update_counter(&credential_id, new_counter as i64, now)
            .map_err(ApiError::from)?;
        super::issue_login_tokens(&state, &user_id)
    }

    pub fn begin_login_assertion(
        state: &AppState,
        user_id: &str,
    ) -> Result<AssertBeginResponse, ApiError> {
        let svc = state.webauthn.as_ref().ok_or_else(webauthn_unconfigured)?;
        let store = WebauthnStore::new(&state.db);
        let rows = store.list_for_user(user_id).map_err(ApiError::from)?;
        let creds: Vec<Passkey> = rows
            .iter()
            .filter_map(|r| serde_json::from_str::<Passkey>(&r.passkey_json).ok())
            .collect();
        if creds.is_empty() {
            return Err(ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "no_passkeys",
                message: "user has no parseable webauthn credentials".into(),
            });
        }
        let (ceremony_id, rcr) = svc
            .begin_authentication(user_id, &creds)
            .map_err(ApiError::from)?;
        let options = serde_json::to_value(&rcr).map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "ceremony_serialise",
            message: e.to_string(),
        })?;
        Ok(AssertBeginResponse {
            webauthn_required: true,
            ceremony_id,
            options,
        })
    }

    pub fn webauthn_unconfigured() -> ApiError {
        ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "webauthn_unconfigured",
            message: "the server has no webauthn relying-party configured".into(),
        }
    }

    pub fn base64_url_encode(bytes: &[u8]) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        URL_SAFE_NO_PAD.encode(bytes)
    }

    pub fn webauthn_router() -> Router<AppState> {
        Router::new()
            .route(
                "/api/admin/webauthn/register/begin",
                post(register_begin_handler),
            )
            .route(
                "/api/admin/webauthn/register/finish",
                post(register_finish_handler),
            )
            .route(
                "/api/admin/webauthn/credentials",
                get(list_credentials_handler),
            )
            .route(
                "/api/admin/webauthn/credentials/{credential_id}",
                delete(delete_credential_handler),
            )
            .route(
                "/api/login/webauthn/finish",
                post(login_assert_finish_handler),
            )
    }
}

// ===========================================================================
// STUB implementation — `webauthn` feature off.
// Routes return 503 webauthn_unconfigured. AppState carries a marker
// so the field type is identical regardless of feature.
// ===========================================================================

#[cfg(not(feature = "webauthn"))]
mod imp {
    use super::*;

    /// Stub WebauthnSvc when the `webauthn` feature is off. Every
    /// method returns `FeatureDisabled` and serialises to a 503.
    /// AppState carries `Option<Arc<WebauthnSvc>>` either way; we just
    /// always set it to `None` in stub mode.
    pub struct WebauthnSvc {
        _phantom: (),
    }

    impl std::fmt::Debug for WebauthnSvc {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("WebauthnSvc(stub)")
        }
    }

    impl WebauthnSvc {
        pub fn new(
            _rp_id: &str,
            _rp_origin: &str,
            _rp_name: &str,
        ) -> Result<Self, WebauthnSvcError> {
            Err(WebauthnSvcError::FeatureDisabled)
        }

        pub fn prune_expired(&self) {}

        #[cfg(test)]
        pub fn pending_count(&self) -> (usize, usize) {
            (0, 0)
        }
    }

    pub fn webauthn_unconfigured() -> ApiError {
        ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "webauthn_unconfigured",
            message: "the server was built without webauthn support".into(),
        }
    }

    pub fn begin_login_assertion(
        _state: &AppState,
        _user_id: &str,
    ) -> Result<AssertBeginResponse, ApiError> {
        Err(webauthn_unconfigured())
    }

    /// Stub handler that returns the structured 503 every webauthn
    /// route emits when the feature is off. Lets the SPA detect the
    /// state by `serverCode === "webauthn_unconfigured"` instead of
    /// hitting a generic 404 from "this URL doesn't exist."
    async fn unconfigured_handler() -> ApiError {
        webauthn_unconfigured()
    }

    pub fn webauthn_router() -> Router<AppState> {
        // Even with the `webauthn` feature off we register the SAME
        // paths so the SPA gets a structured 503 it can recognise
        // (rather than a 404 that's indistinguishable from a typo).
        // The login route's branch never fires anyway because
        // count_for_user always returns 0 — registration is
        // impossible without these handlers actually doing anything.
        use axum::routing::{delete, get, post};
        Router::new()
            .route(
                "/api/admin/webauthn/register/begin",
                post(unconfigured_handler),
            )
            .route(
                "/api/admin/webauthn/register/finish",
                post(unconfigured_handler),
            )
            .route(
                "/api/admin/webauthn/credentials",
                get(unconfigured_handler),
            )
            .route(
                "/api/admin/webauthn/credentials/{credential_id}",
                delete(unconfigured_handler),
            )
            .route(
                "/api/login/webauthn/finish",
                post(unconfigured_handler),
            )
    }
}

pub use imp::{begin_login_assertion, webauthn_router, WebauthnSvc};

// ---------------------------------------------------------------------------
// Tests — only the feature-on path has unit tests; the stub path is
// trivial and covered by the route-level "503 when no svc" check in
// routes.rs.
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "webauthn"))]
mod tests {
    use super::*;
    use webauthn_rs::prelude::RegisterPublicKeyCredential;

    #[test]
    fn build_rejects_invalid_origin() {
        let err = WebauthnSvc::new("localhost", "not-a-url", "execlaw");
        assert!(err.is_err());
    }

    #[test]
    fn build_accepts_localhost_http() {
        let svc =
            WebauthnSvc::new("localhost", "http://localhost:3030", "execlaw").unwrap();
        let (r, a) = svc.pending_count();
        assert_eq!((r, a), (0, 0));
    }

    #[test]
    fn finish_with_unknown_ceremony_id_rejected() {
        let svc =
            WebauthnSvc::new("localhost", "http://localhost:3030", "execlaw").unwrap();
        // Construct any well-formed RegisterPublicKeyCredential; the
        // unknown-id branch fires before the credential is parsed.
        let cred: RegisterPublicKeyCredential = serde_json::from_value(serde_json::json!({
            "id": "abc",
            "rawId": "YWJj",
            "response": {
                "clientDataJSON": "",
                "attestationObject": ""
            },
            "type": "public-key",
            "extensions": {},
        }))
        .expect("placeholder cred parses for the unknown-id branch");
        let err = svc.finish_registration("not-a-real-ceremony", &cred);
        assert!(matches!(err, Err(WebauthnSvcError::UnknownCeremony)));
    }

    #[test]
    fn prune_expired_clears_old_pending() {
        let svc =
            WebauthnSvc::new("localhost", "http://localhost:3030", "execlaw").unwrap();
        let (ceremony_id, _) = svc
            .begin_registration("u1", "alice", "Alice", "k".into(), vec![])
            .unwrap();
        if let Some(mut entry) = svc.pending_register.get_mut(&ceremony_id) {
            entry.expires_at = 0;
        }
        assert_eq!(svc.pending_count().0, 1);
        svc.prune_expired();
        assert_eq!(svc.pending_count().0, 0);
    }
}
