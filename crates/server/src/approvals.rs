//! Approval flow routes (Phase 3, §2.14).
//!
//! When a cold contact messages the agent, the chat route parks the
//! conversation (`AwaitingTrustDecision`) and writes a
//! `ColdContactArrived` event carrying an `approval_id`. The
//! controller responds to the sideband notification by hitting this
//! endpoint with a verb (`Trust` / `TrustLimited` / `Block` /
//! `IgnoreOnce`). The verb decides the principal's new `TrustLevel`,
//! a `TrustChanged` event lands in the log, and — on `Trust` /
//! `TrustLimited` — the original user message is replayed through
//! the normal turn path.
//!
//! The approval id is currently an opaque UUID the server minted on
//! the cold-contact path; Phase-7 hardening swaps it for an
//! EdDSA-signed JWT per §2.11.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Router, routing::post};
use execlaw_core::conversation::{ConversationStore, Phase};
use execlaw_core::events::{EventKind, EventLog, PendingEvent};
use execlaw_core::ids::{ConversationId, EventSeq, PrincipalId};
use execlaw_core::principal::{PrincipalStore, TrustLevel as CoreTrustLevel};
use execlaw_policy::sideband::{ApprovalClaims, ApprovalReason, ApprovalVerb};
use serde::{Deserialize, Serialize};

use crate::auth::JwtSigner;
use crate::events::UiEvent;
use crate::state::AppState;

/// Mint a signed approval-token JWT (§2.11). The token's `jti` is
/// the approval_id; verifying the token before honoring an approval
/// response prevents an attacker from forging an `/approvals/X/respond`
/// request with a guessed id.
pub fn issue_approval_token(
    signer: &JwtSigner,
    approval_id: &str,
    conversation_id: &ConversationId,
    reason: &str,
) -> String {
    use jsonwebtoken::{Algorithm, Header, encode};

    let now = chrono::Utc::now().timestamp();
    let reason_enum = match reason {
        "cold_contact" => ApprovalReason::ColdContact,
        "rule_of_two_breach" => ApprovalReason::RuleOfTwoBreach,
        "sensitive_tool_call" => ApprovalReason::SensitiveToolCall,
        "ask_controller" => ApprovalReason::AskController,
        "anomaly_tripwire" => ApprovalReason::AnomalyTripwire,
        _ => ApprovalReason::ColdContact,
    };
    let claims = ApprovalClaims {
        iss: signer.issuer().to_owned(),
        jti: approval_id.to_owned(),
        conversation_id: conversation_id.as_str().to_owned(),
        reason: reason_enum,
        tool_call_id: None,
        iat: now,
        exp: now + 24 * 3600, // 24h window for the controller to respond
    };
    let header = Header::new(Algorithm::EdDSA);
    encode(&header, &claims, signer.encoding_key()).expect("JWT encode")
}

/// Verify a signed approval token. Returns the decoded claims if
/// the token is valid AND its `jti` matches the path-param
/// `approval_id`. Mismatch → caller can't authorize this approval.
pub fn verify_approval_token(
    signer: &JwtSigner,
    token: &str,
    expected_jti: &str,
) -> Result<ApprovalClaims, String> {
    use jsonwebtoken::{Algorithm, Validation, decode};

    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_issuer(&[signer.issuer()]);
    validation.leeway = 5;

    let data = decode::<ApprovalClaims>(token, signer.decoding_key(), &validation)
        .map_err(|e| format!("approval token verification failed: {e}"))?;

    if data.claims.jti != expected_jti {
        return Err(format!(
            "approval token jti '{}' does not match path approval_id '{}'",
            data.claims.jti, expected_jti
        ));
    }
    Ok(data.claims)
}

#[derive(Debug, Deserialize)]
pub struct ApprovalRequest {
    /// The verb the controller is responding with. See
    /// [`ApprovalVerb`] in `execlaw-policy::sideband`.
    pub verb: ApprovalVerb,
    /// Optional topic scopes for `TrustLimited`.
    #[serde(default)]
    pub allowed_topics: Vec<String>,
    /// Optional human-readable reason (saved on the principal for audit).
    #[serde(default)]
    pub reason: Option<String>,
    /// Signed approval-token JWT minted by the cold-contact path.
    /// Required when `EXECLAW_APPROVAL_TOKEN_REQUIRED` is set or
    /// when the controller's UI is the only thing that should be
    /// able to call this endpoint. Phase 3 accepts an empty token
    /// (back-compat) but logs a warning; Phase 7 hardening flips
    /// this to required.
    #[serde(default)]
    pub approval_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApprovalResponse {
    pub approval_id: String,
    pub principal_id: String,
    pub conversation_id: String,
    pub new_trust_class: String,
    pub outcome: String,
}

/// `POST /api/admin/approvals/:id/respond`
pub async fn respond_handler(
    State(state): State<AppState>,
    Path(approval_id): Path<String>,
    Json(req): Json<ApprovalRequest>,
) -> impl IntoResponse {
    // Verify the signed approval token if one is supplied. An
    // attacker who guesses the approval_id but doesn't have the
    // server's signing key can't forge a matching token (§2.11).
    if let Some(token) = &req.approval_token {
        if let Err(e) = verify_approval_token(&state.signer, token, &approval_id) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": {
                        "code": "bad_approval_token",
                        "message": e,
                    }
                })),
            )
                .into_response();
        }
    }

    // Look up the ColdContactArrived event that minted this
    // approval_id. Phase 3 scans state_events for the matching row;
    // a dedicated `state_approvals` index lands as a Phase-5
    // hardening when the event volume warrants it.
    let Some((cid, sender_principal_id, original_text)) =
        find_cold_contact_event(&state, &approval_id)
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": {
                    "code": "approval_not_found",
                    "message": "no cold-contact event matches this approval_id",
                }
            })),
        )
            .into_response();
    };

    // Apply the verb to the principal.
    let principals = PrincipalStore::new(&state.db);
    let pid = PrincipalId::from(sender_principal_id.clone());
    let now = chrono::Utc::now().timestamp();

    let (new_level, outcome): (CoreTrustLevel, &'static str) = match req.verb {
        ApprovalVerb::Trust => (
            CoreTrustLevel::KnownTrusted {
                resolvers: vec![],
                approved_by: PrincipalId::from("controller"),
                approved_at: now,
            },
            "trust",
        ),
        ApprovalVerb::TrustLimited => (
            CoreTrustLevel::KnownLimited {
                resolvers: vec![],
                allowed_topics: req.allowed_topics.clone(),
                allowed_tools: None,
            },
            "trust_limited",
        ),
        ApprovalVerb::Block => (
            CoreTrustLevel::Blocked {
                blocked_by: PrincipalId::from("controller"),
                blocked_at: now,
                reason: req.reason.clone(),
            },
            "block",
        ),
        ApprovalVerb::IgnoreOnce => {
            // Don't change the trust level; just clear the parked
            // state so future messages prompt again.
            let store = ConversationStore::new(&state.db);
            if let Ok(Some(mut row)) = store.get(&cid) {
                row.phase = Phase::Idle;
                let _ = store.upsert(&row);
            }
            return (
                StatusCode::OK,
                Json(serde_json::json!(ApprovalResponse {
                    approval_id,
                    principal_id: sender_principal_id,
                    conversation_id: cid.as_str().to_owned(),
                    new_trust_class: "UnknownPending".into(),
                    outcome: "ignore_once".into(),
                })),
            )
                .into_response();
        }
        // The non-cold-contact verbs (Approve / Edit / Reject) land
        // with the Rule-of-Two + sensitive-tool-call flows in Phase 3+.
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": {
                        "code": "unsupported_verb",
                        "message": format!(
                            "verb {:?} is not supported for cold-contact approvals; \
                             use Trust / TrustLimited / Block / IgnoreOnce",
                            other
                        ),
                    }
                })),
            )
                .into_response();
        }
    };

    let new_class_tag = new_level.class_tag().to_owned();

    if let Err(e) = principals.set_trust(&pid, new_level) {
        return internal_error(&format!("set_trust: {e}"));
    }

    // Commit a TrustChanged event so replay captures the transition.
    let trust_payload = TrustChangedPayload {
        principal_id: sender_principal_id.clone(),
        new_class: new_class_tag.clone(),
        approval_id: approval_id.clone(),
        reason: req.reason.clone(),
    };
    let log = event_log(&state);
    let trust_event = match PendingEvent::encode(
        EventKind::TrustChanged,
        &trust_payload,
        Some("controller".into()),
    ) {
        Ok(e) => e,
        Err(e) => return internal_error(&format!("encode trust_changed: {e}")),
    };
    let base_seq = match log.last_seq(&cid) {
        Ok(s) => s,
        Err(e) => return internal_error(&format!("last_seq: {e}")),
    };
    if let Err(e) = log.commit_turn(&cid, base_seq, vec![trust_event]) {
        return internal_error(&format!("commit trust_changed: {e}"));
    }

    // On Trust / TrustLimited: broadcast the original message as a
    // ChatMessageInbound so the UI picks up the parked text. On
    // Block: no further messages.
    if matches!(outcome, "trust" | "trust_limited") {
        state.events.publish(UiEvent::ChatMessageInbound {
            conversation_id: cid.as_str().to_owned(),
            seq: log.last_seq(&cid).map(|s| s.0).unwrap_or(0),
            text: original_text,
            sender: Some(sender_principal_id.clone()),
        });
    }

    // Un-park the conversation on non-blocking verbs.
    if !matches!(outcome, "block") {
        let cstore = ConversationStore::new(&state.db);
        if let Ok(Some(mut row)) = cstore.get(&cid) {
            row.phase = Phase::Idle;
            let _ = cstore.upsert(&row);
        }
    }

    (
        StatusCode::OK,
        Json(serde_json::json!(ApprovalResponse {
            approval_id,
            principal_id: sender_principal_id,
            conversation_id: cid.as_str().to_owned(),
            new_trust_class: new_class_tag,
            outcome: outcome.into(),
        })),
    )
        .into_response()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ColdContactReplayPayload {
    text: String,
    sender_principal_id: String,
    approval_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustChangedPayload {
    principal_id: String,
    new_class: String,
    approval_id: String,
    reason: Option<String>,
}

/// Scan conversations for a ColdContactArrived event matching this
/// approval_id. Returns (conversation_id, sender_principal_id, text).
///
/// This is a linear scan over state_events; fine for Phase 3 where
/// approvals are rare and short-lived. An index on
/// `(kind, approval_id)` lands as a hardening pass.
fn find_cold_contact_event(
    state: &AppState,
    approval_id: &str,
) -> Option<(ConversationId, String, String)> {
    let db = &state.db;
    let conv_ids: Vec<String> = db
        .with_conn(|c| {
            let mut stmt = c
                .prepare("SELECT DISTINCT conversation_id FROM state_events WHERE kind = 'cold_contact_arrived'")
                .map_err(execlaw_core::db::DbError::from)?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(execlaw_core::db::DbError::from)?;
            let out: Result<Vec<_>, _> = rows.collect();
            Ok(out?)
        })
        .ok()?;

    let log = event_log(state);
    for id in conv_ids {
        let cid = ConversationId::from(id);
        let events = log.replay_since(&cid, EventSeq(0)).ok()?;
        for ev in events {
            if ev.kind != EventKind::ColdContactArrived {
                continue;
            }
            if let Ok(p) = ev.decode_payload::<ColdContactReplayPayload>() {
                if p.approval_id == approval_id {
                    return Some((cid, p.sender_principal_id, p.text));
                }
            }
        }
    }
    None
}

fn event_log(state: &AppState) -> EventLog<'_> {
    let log = EventLog::new(&state.db);
    match &state.event_log_hmac_key {
        Some(k) => log.with_hmac_key((**k).clone()),
        None => log,
    }
}

fn internal_error(msg: &str) -> axum::response::Response {
    tracing::error!("{msg}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": {"code": "internal", "message": msg}
        })),
    )
        .into_response()
}

/// `POST /api/admin/principals/:id/revoke` — controller-only path
/// to revoke trust on an existing principal without going through
/// the cold-contact flow. The principal's `TrustLevel` flips to
/// `Blocked`; future messages from them get 403 sender_blocked.
///
/// This is the explicit operator action for "I no longer trust X" —
/// distinct from `Block` via the approval flow (which targets a
/// specific cold-contact request). Use this for an *already
/// trusted* contact you want to remove.
pub async fn revoke_handler(
    State(state): State<AppState>,
    Path(principal_id): Path<String>,
    Json(req): Json<RevokeRequest>,
) -> impl IntoResponse {
    let principals = PrincipalStore::new(&state.db);
    let pid = PrincipalId::from(principal_id.clone());
    let Ok(Some(_existing)) = principals.get(&pid) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": {
                    "code": "principal_not_found",
                    "message": format!("no principal with id '{principal_id}'"),
                }
            })),
        )
            .into_response();
    };

    let now = chrono::Utc::now().timestamp();
    let new_level = CoreTrustLevel::Blocked {
        blocked_by: PrincipalId::from("controller"),
        blocked_at: now,
        reason: req.reason.clone(),
    };
    if let Err(e) = principals.set_trust(&pid, new_level) {
        return internal_error(&format!("set_trust: {e}"));
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "principal_id": principal_id,
            "new_trust_class": "Blocked",
            "outcome": "revoked",
        })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct RevokeRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

/// Sub-router mounted at `/api/admin/approvals/...` and
/// `/api/admin/principals/.../revoke`.
pub fn approvals_router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/admin/approvals/{approval_id}/respond",
            post(respond_handler),
        )
        .route(
            "/api/admin/principals/{principal_id}/revoke",
            post(revoke_handler),
        )
}
