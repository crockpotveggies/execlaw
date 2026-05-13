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
use axum::{
    Router,
    routing::{get, post},
};
use execlaw_core::conversation::{ConversationStore, Phase};
use execlaw_core::events::{EventKind, EventLog, PendingEvent};
use execlaw_core::ids::{ConversationId, EventSeq, PrincipalId};
use execlaw_core::principal::{PrincipalStore, TrustLevel as CoreTrustLevel};
use execlaw_policy::sideband::{ApprovalClaims, ApprovalReason, ApprovalVerb};
use serde::{Deserialize, Serialize};

use crate::auth::JwtSigner;
use crate::events::UiEvent;
use crate::state::AppState;

/// Look up the conversation's originating-transport channel by
/// asking the host's transport-binding store + transport registry.
/// Used by the cold-contact and claim_as_me approval replays so
/// they don't hardcode which channel the inbound came in on
/// (previously this was always "signal", which broke the moment
/// other transports started seeing cold-contact flows).
///
/// Returns `None` when:
///   * The conversation isn't tied to a principal_group (web-only
///     chat with no transport);
///   * No bindings exist yet for that group;
///   * No installed plugin handles any of the bindings' channels
///     (e.g. operator uninstalled the transport between the cold-
///     contact arriving and the approval being processed).
///
/// Callers thread the result into `dispatch_external_turn`'s
/// `origin_channel` so the auto-bridge / typing indicator / system
/// prose all surface the right transport.
fn origin_channel_for_conversation(state: &AppState, cid: &ConversationId) -> Option<String> {
    use execlaw_core::principal_groups::PrincipalGroupStore;
    use execlaw_core::transport_bindings::TransportBindingStore;
    let pg_store = PrincipalGroupStore::new(&state.db);
    // 2026-05-13 — log DB errors loudly instead of `.ok()`-
    // swallowing them. Pre-rework a `principal_groups` or
    // `transport_bindings` decode failure silently degraded the
    // approval reply to "no transport bridged this message" with
    // no diagnostic, which is indistinguishable from a legitimately
    // web-only chat. The WARN now distinguishes the two.
    let pg_id = match pg_store.principal_group_id_for(cid.as_str()) {
        Ok(opt) => opt?,
        Err(e) => {
            tracing::warn!(
                target: "approvals",
                conversation_id = %cid.as_str(),
                error = %e,
                "principal_groups read failed — approval reply will not be bridged via transport. \
                 Likely BLOB column corruption; check Settings → Groups.",
            );
            return None;
        }
    };
    let binding_store = TransportBindingStore::new(&state.db);
    let bindings = match binding_store.bindings_for_group_any_channel(&pg_id) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "approvals",
                principal_group_id = %pg_id,
                error = %e,
                "transport_bindings read failed — approval reply will not be bridged via transport.",
            );
            return None;
        }
    };
    state
        .host_transports
        .lookup_first_supported_binding(&bindings)
        .map(|r| r.channel)
}

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

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ApprovalRequest {
    /// The verb the controller is responding with. See
    /// [`ApprovalVerb`] in `execlaw-policy::sideband`.
    #[schema(value_type = String)]
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
#[utoipa::path(
    post,
    path = "/api/admin/approvals/{approval_id}/respond",
    params(
        ("approval_id" = String, Path, description = "Pending approval id"),
    ),
    responses(
        (status = 200, description = "Approval recorded; original action resumed (or dropped)"),
        (status = 401, description = "Missing or invalid signed approval token"),
        (status = 404, description = "Unknown approval id"),
    ),
    tag = "approvals"
)]
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
            state.events.publish(UiEvent::ApprovalResolved {
                approval_id: approval_id.clone(),
                conversation_id: cid.as_str().to_owned(),
            });
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
        ApprovalVerb::ClaimAsMe => {
            return claim_as_me(state, approval_id, &cid, &pid, original_text).await;
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
                             use Trust / TrustLimited / ClaimAsMe / Block / IgnoreOnce",
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
            text: original_text.clone(),
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

    // Replay the queued message through the agent so the controller
    // doesn't have to ask the contact to re-send. Pre-fix, the
    // approval flipped trust + un-parked the conversation but no
    // turn ever ran — the contact would just see "you read this 5
    // minutes ago" with no reply. Now we dispatch_external_turn
    // with the freshly-promoted principal so the agent answers
    // what was actually asked.
    if matches!(outcome, "trust" | "trust_limited") {
        let promoted = match principals.get(&pid) {
            Ok(Some(p)) => p,
            _ => {
                return internal_error("post-trust principal lookup failed");
            }
        };
        let trust_flat = execlaw_policy::trust::TrustLevel::parse(promoted.trust_level.class_tag())
            .unwrap_or(execlaw_policy::trust::TrustLevel::UnknownPending);
        // Look up the actual originating channel from the
        // conversation's first transport binding. Previously this
        // was hardcoded to "signal" because Signal was the only
        // shipped transport with a cold-contact UX; now any
        // installed transport (sms, whatsapp, slack, ...) gets the
        // right origin channel without code changes here.
        let origin_channel = origin_channel_for_conversation(&state, &cid);
        // Post-approval replay: the controller has explicitly trusted
        // this contact, so the addressing question is moot — fall
        // through with `EligibilityBypass`. The conversation may
        // still be a group, in which case the resolver returns
        // Some(...) and the agent gets the same room-awareness it
        // would on a directly addressed inbound.
        let replay_group_ctx = crate::chats::resolve_group_turn_context(
            &state,
            &cid,
            crate::group_addressing::AddressedReason::EligibilityBypass,
        );
        if let Err(e) = crate::chats::dispatch_external_turn(
            &state,
            &cid,
            &promoted,
            trust_flat,
            &original_text,
            origin_channel.as_deref(),
            replay_group_ctx,
        )
        .await
        {
            tracing::warn!(
                target: "approvals",
                conversation_id = %cid.as_str(),
                error = %e,
                "post-approval replay through dispatch_external_turn failed; \
                 trust transition stands but the agent didn't run a turn — \
                 operator can ask the contact to re-send",
            );
        }
    }

    state.events.publish(UiEvent::ApprovalResolved {
        approval_id: approval_id.clone(),
        conversation_id: cid.as_str().to_owned(),
    });

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

/// "This is me" — the controller is messaging the agent from a
/// not-yet-registered handle. Adds the cold-contact's
/// `(transport, handle)` to the controller's identifiers, then
/// calls `principal_admit::reconcile_against_my_identities` which
/// merges the stale UnknownPending principal into the controller,
/// rebinds the binding + conversation, and unsticks the
/// `awaiting_trust_decision` phase. After reconcile the queued
/// message is dispatched through the controller turn handler so
/// the agent answers immediately.
async fn claim_as_me(
    state: AppState,
    approval_id: String,
    cid: &ConversationId,
    stale_pid: &PrincipalId,
    original_text: String,
) -> axum::response::Response {
    let principals = PrincipalStore::new(&state.db);
    let now = chrono::Utc::now().timestamp();

    // 1. Pull the stale principal's identifiers. If the principal is
    // already gone, that means reconcile ran between the cold-contact
    // arriving and the operator clicking — the resolution we'd
    // perform here has already happened (or is moot). Idempotent
    // 200: log the situation, replay the queued message as a
    // Controller turn (the binding now points at the controller's
    // group), and return success so the SPA refreshes its list and
    // drops the phantom approval entry.
    let stale = match principals.get(stale_pid) {
        Ok(Some(p)) => Some(p),
        Ok(None) => {
            tracing::info!(
                target: "approvals::claim_as_me",
                approval_id = %approval_id,
                stale_pid = %stale_pid.as_str(),
                "cold-contact principal already reconciled away — replaying queued message only",
            );
            None
        }
        Err(e) => return internal_error(&format!("principal get: {e}")),
    };

    // 2. Resolve the controller principal id via the same accessor
    // `add_my_identifier` uses. Lazy-create when the controller's
    // explicit row doesn't exist yet (matches the existing My-
    // identities semantics).
    let controller_pid = match crate::routes::controller_principal_id(&state.db) {
        Ok(p) => p,
        Err(e) => return internal_error(&format!("controller principal: {}", e.message)),
    };
    let mut controller_row = match principals.get(&controller_pid) {
        Ok(Some(p)) => p,
        Ok(None) => execlaw_core::principal::Principal {
            id: controller_pid.clone(),
            identifiers: Vec::new(),
            trust_level: CoreTrustLevel::Controller,
            resolved_by: Vec::new(),
            metadata: serde_json::json!({}),
            first_seen: now,
            last_seen: Some(now),
            controller_notes: None,
        },
        Err(e) => return internal_error(&format!("controller principal lookup: {e}")),
    };

    // 3. Add every identifier from the stale principal to the
    // controller (deduped). The reconcile pass below will then
    // merge the stale principal away, rebind the conversation,
    // and flip the awaiting_trust_decision phase to idle.
    // When `stale` is None (already reconciled), this loop is a
    // no-op — the controller already owns the identifier.
    let mut added_any = false;
    if let Some(stale_row) = stale.as_ref() {
        for ident in &stale_row.identifiers {
            if !controller_row.identifiers.contains(ident) {
                controller_row.identifiers.push(ident.clone());
                added_any = true;
            }
        }
    }
    if added_any {
        controller_row.last_seen = Some(now);
        if let Err(e) = principals.upsert(&controller_row) {
            return internal_error(&format!("controller upsert: {e}"));
        }
    }

    // 4. Reconcile.
    let report = match crate::principal_admit::reconcile_against_my_identities(&state.db) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "approvals::claim_as_me",
                error = %e,
                "reconcile after claim_as_me failed; controller now owns the identifier \
                 but the stale principal may still shadow it",
            );
            crate::principal_admit::ReconcileReport::default()
        }
    };
    tracing::info!(
        target: "approvals::claim_as_me",
        approval_id = %approval_id,
        merged = report.merged.len(),
        bindings_repointed = report.bindings_repointed,
        conversations_repointed = report.conversations_repointed,
        "claim_as_me reconcile complete",
    );

    // 5. Replay the queued message as the controller. After
    // reconcile the conversation is bound to the controller's
    // group; dispatch_external_turn with the controller principal
    // runs the same pipeline a fresh inbound from the controller
    // would.
    let controller_now = match principals.get(&controller_pid) {
        Ok(Some(p)) => p,
        _ => return internal_error("controller principal vanished mid-claim"),
    };
    let trust_flat = execlaw_policy::trust::TrustLevel::Controller;
    // Look up the actual origin channel; see
    // origin_channel_for_conversation for rationale.
    let origin_channel = origin_channel_for_conversation(&state, cid);
    // Same rationale as the post-approval replay: the controller
    // explicitly claimed this turn, so addressing is moot — but
    // resolve the group context anyway so a controller-claimed
    // turn in a Signal group still gets the room-awareness block.
    let claim_group_ctx = crate::chats::resolve_group_turn_context(
        &state,
        cid,
        crate::group_addressing::AddressedReason::EligibilityBypass,
    );
    if let Err(e) = crate::chats::dispatch_external_turn(
        &state,
        cid,
        &controller_now,
        trust_flat,
        &original_text,
        origin_channel.as_deref(),
        claim_group_ctx,
    )
    .await
    {
        tracing::warn!(
            target: "approvals::claim_as_me",
            conversation_id = %cid.as_str(),
            error = %e,
            "dispatch_external_turn replay failed after claim_as_me; \
             trust transition stands but no turn ran",
        );
    }

    state.events.publish(UiEvent::ApprovalResolved {
        approval_id: approval_id.clone(),
        conversation_id: cid.as_str().to_owned(),
    });

    (
        StatusCode::OK,
        Json(serde_json::json!(ApprovalResponse {
            approval_id,
            principal_id: controller_pid.as_str().to_owned(),
            conversation_id: cid.as_str().to_owned(),
            new_trust_class: "Controller".into(),
            outcome: "claim_as_me".into(),
        })),
    )
        .into_response()
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
    // 2026-05-13 — log DB errors at WARN instead of `.ok()?`-
    // swallowing them. Pre-rework a `state_events` query failure
    // (lock contention, schema mismatch, HMAC reverification error)
    // silently degraded to "approval not found", which the caller
    // surfaces as a generic 404 — operators chasing a missing
    // approval had no signal that the underlying read failed.
    let db = &state.db;
    let conv_ids: Vec<String> = match db.with_conn(|c| {
        let mut stmt = c
            .prepare("SELECT DISTINCT conversation_id FROM state_events WHERE kind = 'cold_contact_arrived'")
            .map_err(execlaw_core::db::DbError::from)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(execlaw_core::db::DbError::from)?;
        let out: Result<Vec<_>, _> = rows.collect();
        Ok(out?)
    }) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target: "approvals::find_cold_contact_event",
                approval_id = %approval_id,
                error = %e,
                "state_events scan failed — caller will see 'approval not found'. \
                 Likely DB lock contention or schema drift.",
            );
            return None;
        }
    };

    let log = event_log(state);
    for id in conv_ids {
        let cid = ConversationId::from(id);
        let events = match log.replay_since(&cid, EventSeq(0)) {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!(
                    target: "approvals::find_cold_contact_event",
                    approval_id = %approval_id,
                    conversation_id = %cid.as_str(),
                    error = %e,
                    "replay_since failed for one conversation — continuing scan.",
                );
                continue;
            }
        };
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
#[utoipa::path(
    post,
    path = "/api/admin/principals/{principal_id}/revoke",
    params(
        ("principal_id" = String, Path, description = "Principal to flip to Blocked"),
    ),
    responses(
        (status = 200, description = "Trust revoked; principal now Blocked"),
        (status = 404, description = "Unknown principal id"),
    ),
    tag = "approvals"
)]
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

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct RevokeRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// List endpoints — read-only feeds for the SPA's settings pages.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PrincipalSummary {
    pub id: String,
    pub trust_class: String,
    pub display_name: Option<String>,
    pub first_seen: i64,
    pub last_seen: Option<i64>,
    pub identifiers: Vec<IdentifierSummary>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct IdentifierSummary {
    pub transport: String,
    pub handle: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PrincipalListResponse {
    pub principals: Vec<PrincipalSummary>,
}

/// `GET /api/admin/principals` — every principal the system has seen,
/// ordered by first_seen ascending (Controller first since it was
/// minted at setup time).
#[utoipa::path(
    get,
    path = "/api/admin/principals",
    responses(
        (status = 200, description = "Principal summaries", body = PrincipalListResponse),
        (status = 401, description = "Missing or invalid Authorization header"),
    ),
    security(("bearer_jwt" = [])),
    tag = "approvals"
)]
pub async fn list_principals_handler(
    State(state): State<AppState>,
    _user: crate::auth_extract::AuthedUser,
) -> impl IntoResponse {
    let store = PrincipalStore::new(&state.db);
    let principals = match store.list_all() {
        Ok(p) => p,
        Err(e) => return internal_error(&format!("list_all: {e}")),
    };
    let summaries: Vec<PrincipalSummary> = principals
        .into_iter()
        .map(|p| PrincipalSummary {
            id: p.id.as_str().to_owned(),
            trust_class: p.trust_level.class_tag().to_owned(),
            display_name: p
                .metadata
                .get("display_name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned()),
            first_seen: p.first_seen,
            last_seen: p.last_seen,
            identifiers: p
                .identifiers
                .into_iter()
                .map(|i| IdentifierSummary {
                    transport: i.transport,
                    handle: i.handle,
                })
                .collect(),
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!(PrincipalListResponse {
            principals: summaries
        })),
    )
        .into_response()
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PendingApprovalSummary {
    pub approval_id: String,
    pub conversation_id: String,
    pub sender_principal_id: String,
    pub original_text: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PendingApprovalsResponse {
    pub approvals: Vec<PendingApprovalSummary>,
}

/// `GET /api/admin/approvals` — every cold-contact arrival whose
/// sender is still `UnknownPending`. Linear scan via state_events;
/// acceptable while approval volume is low (Phase-3 scope).
#[utoipa::path(
    get,
    path = "/api/admin/approvals",
    responses(
        (status = 200, description = "Pending approvals", body = PendingApprovalsResponse),
        (status = 401, description = "Missing or invalid Authorization header"),
    ),
    security(("bearer_jwt" = [])),
    tag = "approvals"
)]
pub async fn list_pending_approvals_handler(
    State(state): State<AppState>,
    _user: crate::auth_extract::AuthedUser,
) -> impl IntoResponse {
    let principals = PrincipalStore::new(&state.db);

    // Pull every distinct conversation that has at least one
    // cold_contact_arrived event.
    let conv_ids: Vec<String> = match state.db.with_conn(|c| {
        let mut stmt = c
            .prepare(
                "SELECT DISTINCT conversation_id FROM state_events \
                 WHERE kind = 'cold_contact_arrived'",
            )
            .map_err(execlaw_core::db::DbError::from)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(execlaw_core::db::DbError::from)?;
        let out: Result<Vec<_>, _> = rows.collect();
        Ok(out?)
    }) {
        Ok(v) => v,
        Err(e) => return internal_error(&format!("conv scan: {e}")),
    };

    let log = event_log(&state);
    let mut approvals: Vec<PendingApprovalSummary> = Vec::new();
    for cid_str in conv_ids {
        let cid = ConversationId::from(cid_str.clone());
        let events = match log.replay_since(&cid, EventSeq(0)) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for ev in events {
            if ev.kind != EventKind::ColdContactArrived {
                continue;
            }
            let Ok(p) = ev.decode_payload::<ColdContactReplayPayload>() else {
                continue;
            };
            // Filter out approvals that are no longer actionable:
            //   * principal still exists AND is UnknownPending → pending
            //   * principal still exists AND is anything else → resolved
            //     (operator already approved/blocked, or trust changed
            //     via another path)
            //   * principal MISSING → reconciled away. Pre-fix this
            //     defaulted to `pending=true`, so a cold-contact whose
            //     principal got merged into the controller via the
            //     My-identities reconcile flow kept appearing as a
            //     phantom approval — clicking any verb errored
            //     because the principal was gone, leaving the
            //     operator stuck. The right answer is to drop it:
            //     a vanished principal IS the resolution.
            let principal_state = principals
                .get(&PrincipalId::from(p.sender_principal_id.clone()))
                .ok()
                .flatten();
            let still_pending = match principal_state {
                Some(principal) => {
                    matches!(principal.trust_level, CoreTrustLevel::UnknownPending { .. })
                }
                None => false,
            };
            if !still_pending {
                continue;
            }
            approvals.push(PendingApprovalSummary {
                approval_id: p.approval_id,
                conversation_id: cid_str.clone(),
                sender_principal_id: p.sender_principal_id,
                original_text: p.text,
            });
        }
    }
    (
        StatusCode::OK,
        Json(serde_json::json!(PendingApprovalsResponse { approvals })),
    )
        .into_response()
}

/// Sub-router mounted at `/api/admin/approvals/...` and
/// `/api/admin/principals/.../revoke`.
pub fn approvals_router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/approvals", get(list_pending_approvals_handler))
        .route("/api/admin/principals", get(list_principals_handler))
        .route(
            "/api/admin/approvals/{approval_id}/respond",
            post(respond_handler),
        )
        .route(
            "/api/admin/principals/{principal_id}/revoke",
            post(revoke_handler),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::{self, Body};
    use axum::http::{Method, Request, header};
    use tower::ServiceExt;

    async fn setup_get_token(app: &axum::Router) -> String {
        let body = serde_json::to_vec(&serde_json::json!({
            "username": "tester",
            "admin_password": "hunter2-longer",
            "display_name": "Tester",
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

    async fn read_json(
        app: &axum::Router,
        token: Option<&str>,
        uri: &str,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(Method::GET).uri(uri);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let resp = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    #[tokio::test]
    async fn list_principals_requires_auth() {
        let app = build_router(test_app_state());
        let (status, _) = read_json(&app, None, "/api/admin/principals").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn list_principals_returns_empty_on_fresh_db() {
        let app = build_router(test_app_state());
        let token = setup_get_token(&app).await;
        let (status, body) = read_json(&app, Some(&token), "/api/admin/principals").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["principals"].is_array());
        assert_eq!(body["principals"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn list_pending_approvals_requires_auth() {
        let app = build_router(test_app_state());
        let (status, _) = read_json(&app, None, "/api/admin/approvals").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn list_pending_approvals_returns_empty_on_fresh_db() {
        let app = build_router(test_app_state());
        let token = setup_get_token(&app).await;
        let (status, body) = read_json(&app, Some(&token), "/api/admin/approvals").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["approvals"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn list_pending_filters_out_events_whose_principal_was_reconciled_away() {
        // Regression: when reconcile (My-identities flow) deleted a
        // stale UnknownPending principal, the cold_contact_arrived
        // events for it stayed in the immutable event log. The
        // pre-fix list filter defaulted "missing principal" to
        // pending=true, so these phantoms kept appearing in the
        // approvals list and the SPA's "This is me" / Trust /
        // Block clicks all errored because the principal was gone.
        // Post-fix: a vanished principal IS the resolution; drop
        // the event from the list.
        use execlaw_core::events::{EventKind, PendingEvent};
        let state = test_app_state();
        let app = build_router(state.clone());
        let token = setup_get_token(&app).await;

        // Seed a cold_contact_arrived event whose principal_id
        // points at a row that doesn't exist (simulates post-
        // reconcile state).
        let cid = ConversationId::from("conv-phantom".to_owned());
        let payload = ColdContactReplayPayload {
            text: "Good morning".to_owned(),
            sender_principal_id: "pri_signal_+15551234567".to_owned(),
            approval_id: "appr-phantom-1".to_owned(),
        };
        let pending = PendingEvent::encode(EventKind::ColdContactArrived, &payload, None).unwrap();
        let log = event_log(&state);
        log.commit_turn(&cid, EventSeq(0), vec![pending]).unwrap();

        // Verify the principal IS missing (no upsert; the test
        // simulates "reconcile already deleted it").
        let principals = PrincipalStore::new(&state.db);
        assert!(
            principals
                .get(&PrincipalId::from(payload.sender_principal_id.clone()))
                .unwrap()
                .is_none()
        );

        // Pending-list endpoint must NOT surface this event.
        let (status, body) = read_json(&app, Some(&token), "/api/admin/approvals").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["approvals"].as_array().unwrap().len(),
            0,
            "phantom approval (principal deleted by reconcile) must not appear in pending list"
        );
    }

    #[tokio::test]
    async fn list_pending_keeps_events_whose_principal_is_unknown_pending() {
        // Counterpart to the phantom-filter test: when the principal
        // EXISTS and is genuinely UnknownPending, the cold-contact
        // event should appear in the pending list.
        use execlaw_core::events::{EventKind, PendingEvent};
        use execlaw_core::principal::{Principal, TrustLevel as CoreTrustLevel};

        let state = test_app_state();
        let app = build_router(state.clone());
        let token = setup_get_token(&app).await;

        let principals = PrincipalStore::new(&state.db);
        let pid = PrincipalId::from("pri_signal_+15559998888");
        principals
            .upsert(&Principal {
                id: pid.clone(),
                identifiers: Vec::new(),
                trust_level: CoreTrustLevel::UnknownPending {
                    first_seen: 0,
                    notification_event_seq: None,
                },
                resolved_by: Vec::new(),
                metadata: serde_json::json!({}),
                first_seen: 0,
                last_seen: None,
                controller_notes: None,
            })
            .unwrap();

        let cid = ConversationId::from("conv-real".to_owned());
        let payload = ColdContactReplayPayload {
            text: "hi can we chat".to_owned(),
            sender_principal_id: pid.as_str().to_owned(),
            approval_id: "appr-real-1".to_owned(),
        };
        let pending = PendingEvent::encode(EventKind::ColdContactArrived, &payload, None).unwrap();
        let log = event_log(&state);
        log.commit_turn(&cid, EventSeq(0), vec![pending]).unwrap();

        let (status, body) = read_json(&app, Some(&token), "/api/admin/approvals").await;
        assert_eq!(status, StatusCode::OK);
        let arr = body["approvals"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["approval_id"], "appr-real-1");
    }
}
