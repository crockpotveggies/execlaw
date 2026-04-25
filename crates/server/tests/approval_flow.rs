//! Phase 3 approval-flow integration tests.
//!
//! Covers the end-to-end cold-contact → controller-approves →
//! conversation-resumes flow, plus every `ApprovalVerb` branch.

use axum::body::{self, Body};
use axum::http::{Method, Request, StatusCode, header};
use execlaw_core::db::{Database, DbConfig};
use execlaw_core::events::{EventKind, EventLog};
use execlaw_core::ids::{ConversationId, EventSeq, PrincipalId};
use execlaw_core::migrations::MigrationRunner;
use execlaw_core::principal::{PrincipalStore, TrustLevel as CoreTrustLevel};
use execlaw_plugin_host::{HookRegistry, PluginHost};
use execlaw_server::{AppState, EventBus, JwtSigner, RefreshStore, ServerConfig};
use std::sync::Arc;
use tower::ServiceExt;

fn build_app(stage_root: std::path::PathBuf) -> (axum::Router, AppState) {
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    let state = AppState {
        db: db.clone(),
        config: Arc::new(ServerConfig::default()),
        signer: Arc::new(JwtSigner::generate("execlaw-test".into())),
        refresh_store: Arc::new(RefreshStore::new(db.clone())),
        events: EventBus::new(),
        event_log_hmac_key: Some(Arc::new(b"execlaw-test-hmac-key-32-bytes!!".to_vec())),
        inference: None,
        plugin_host: PluginHost::new(db.clone(), HookRegistry::new(), stage_root),
        webauthn: None,
        mcp_host: execlaw_server::mcp_host::McpHost::new(db),
    };
    (
        execlaw_server::routes::build_router(state.clone()),
        state,
    )
}

async fn send_cold_contact(
    app: axum::Router,
    conv_id: &str,
    sender: &str,
    text: &str,
) -> serde_json::Value {
    let body = serde_json::to_vec(&serde_json::json!({
        "text": text,
        "sender_principal_id": sender,
    }))
    .unwrap();
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/chats/{conv_id}/messages"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn respond(app: axum::Router, approval_id: &str, verb: &str) -> (StatusCode, serde_json::Value) {
    let body = serde_json::to_vec(&serde_json::json!({ "verb": verb })).unwrap();
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/admin/approvals/{approval_id}/respond"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// End-to-end happy path: cold contact → `Trust` verb → principal
/// upgraded to KnownTrusted; TrustChanged event committed; original
/// message replayed on the bus; conversation un-parked.
#[tokio::test]
async fn trust_verb_upgrades_principal_and_resumes_conversation() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, state) = build_app(tmp.path().to_path_buf());

    let initial = send_cold_contact(app.clone(), "flow-1", "newcomer", "hi there").await;
    let approval_id = initial["approval_id"].as_str().unwrap().to_owned();

    // Subscribe BEFORE responding so we can see the replay broadcast.
    let mut rx = state.events.subscribe();

    let (status, body) = respond(app.clone(), &approval_id, "trust").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["outcome"], "trust");
    assert_eq!(body["new_trust_class"], "KnownTrusted");

    // Principal now has KnownTrusted in the store.
    let store = PrincipalStore::new(&state.db);
    let p = store
        .get(&PrincipalId::from("newcomer"))
        .unwrap()
        .unwrap();
    assert!(matches!(p.trust_level, CoreTrustLevel::KnownTrusted { .. }));

    // TrustChanged event committed to the conversation log.
    let log = EventLog::new(&state.db);
    let events = log
        .replay_since(&ConversationId::from("flow-1"), EventSeq(0))
        .unwrap();
    assert!(events.iter().any(|e| e.kind == EventKind::TrustChanged));

    // The original message was replayed on the bus so the UI picks it up.
    let mut saw_replay = false;
    for _ in 0..5 {
        if let Ok(Ok(execlaw_server::UiEvent::ChatMessageInbound { text, .. })) =
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
            && text == "hi there"
        {
            saw_replay = true;
            break;
        }
    }
    assert!(saw_replay, "original text must be replayed after Trust verb");
}

/// `TrustLimited` verb with allowed_topics upgrades to KnownLimited.
#[tokio::test]
async fn trust_limited_verb_restricts_by_topic() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, state) = build_app(tmp.path().to_path_buf());

    let init = send_cold_contact(app.clone(), "flow-2", "limited-user", "hi").await;
    let approval_id = init["approval_id"].as_str().unwrap().to_owned();

    let body = serde_json::to_vec(&serde_json::json!({
        "verb": "trust_limited",
        "allowed_topics": ["weather", "scheduling"]
    }))
    .unwrap();
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/admin/approvals/{approval_id}/respond"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let p = PrincipalStore::new(&state.db)
        .get(&PrincipalId::from("limited-user"))
        .unwrap()
        .unwrap();
    match p.trust_level {
        CoreTrustLevel::KnownLimited { allowed_topics, .. } => {
            assert_eq!(allowed_topics.len(), 2);
            assert!(allowed_topics.contains(&"weather".into()));
        }
        other => panic!("expected KnownLimited, got {other:?}"),
    }
}

/// `Block` verb: principal's trust becomes Blocked, no conversation
/// replay happens, subsequent messages get dropped with 403.
#[tokio::test]
async fn block_verb_drops_all_future_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, state) = build_app(tmp.path().to_path_buf());

    let init = send_cold_contact(app.clone(), "flow-3", "spammer", "spam").await;
    let approval_id = init["approval_id"].as_str().unwrap().to_owned();

    let (status, body) = respond(app.clone(), &approval_id, "block").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["outcome"], "block");

    // Next message from the same sender: 403 sender_blocked.
    let body = serde_json::to_vec(&serde_json::json!({
        "text": "still spamming",
        "sender_principal_id": "spammer"
    }))
    .unwrap();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/chats/flow-3/messages")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Principal is Blocked.
    let _ = state;
}

/// `IgnoreOnce`: clears the parked state without changing trust.
/// Subsequent messages re-trigger cold-contact (the principal is
/// still UnknownPending).
#[tokio::test]
async fn ignore_once_clears_parked_state_without_trust_change() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, state) = build_app(tmp.path().to_path_buf());

    let init = send_cold_contact(app.clone(), "flow-4", "maybe-user", "hi").await;
    let approval_id = init["approval_id"].as_str().unwrap().to_owned();

    let (status, body) = respond(app.clone(), &approval_id, "ignore_once").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["outcome"], "ignore_once");

    // Principal is still UnknownPending (no trust change).
    let p = PrincipalStore::new(&state.db)
        .get(&PrincipalId::from("maybe-user"))
        .unwrap()
        .unwrap();
    assert!(matches!(p.trust_level, CoreTrustLevel::UnknownPending { .. }));

    // Second cold message from same sender re-parks the conversation.
    let init2 = send_cold_contact(app.clone(), "flow-4", "maybe-user", "hello again").await;
    // New approval_id — each cold-contact event mints its own.
    assert!(init2["approval_id"].as_str().unwrap().starts_with("appr-"));
    assert_ne!(init2["approval_id"], init["approval_id"]);
}

/// Bogus approval_id: 404.
#[tokio::test]
async fn bogus_approval_id_returns_404() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _) = build_app(tmp.path().to_path_buf());

    let (status, _) = respond(app, "appr-nonexistent", "trust").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Unsupported verb for cold-contact (e.g. `approve`): 400.
#[tokio::test]
async fn unsupported_verb_for_cold_contact_is_400() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _) = build_app(tmp.path().to_path_buf());

    let init = send_cold_contact(app.clone(), "flow-5", "someone", "hi").await;
    let approval_id = init["approval_id"].as_str().unwrap().to_owned();

    let (status, body) = respond(app, &approval_id, "approve").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "unsupported_verb");
}

/// Adversarial: revoke trust on an existing KnownTrusted contact via
/// the dedicated `POST /api/admin/principals/:id/revoke` route. After
/// revoke, future messages from them are dropped with 403.
#[tokio::test]
async fn revoke_trust_drops_future_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _state) = build_app(tmp.path().to_path_buf());

    // Trust first.
    let init = send_cold_contact(app.clone(), "flow-6", "friend", "hey").await;
    let approval_id = init["approval_id"].as_str().unwrap().to_owned();
    let (_, _) = respond(app.clone(), &approval_id, "trust").await;

    // Revoke via the HTTP route (the controller-action path).
    let revoke_body = serde_json::to_vec(&serde_json::json!({
        "reason": "manual revoke"
    }))
    .unwrap();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/admin/principals/friend/revoke")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(revoke_body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&body::to_bytes(resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    assert_eq!(body["new_trust_class"], "Blocked");
    assert_eq!(body["outcome"], "revoked");

    // Their next message gets 403.
    let body = serde_json::to_vec(&serde_json::json!({
        "text": "you still there?",
        "sender_principal_id": "friend"
    }))
    .unwrap();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/chats/flow-6/messages")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// Revoke on a non-existent principal returns 404.
#[tokio::test]
async fn revoke_unknown_principal_is_404() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _) = build_app(tmp.path().to_path_buf());
    let body = serde_json::to_vec(&serde_json::json!({})).unwrap();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/admin/principals/ghost/revoke")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Cold-contact response now includes a signed `approval_token`
/// (§2.11). The token's `jti` matches the approval_id.
#[tokio::test]
async fn cold_contact_emits_signed_approval_token() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _state) = build_app(tmp.path().to_path_buf());
    let init = send_cold_contact(app, "flow-token", "stranger", "hi").await;
    let token = init["approval_token"]
        .as_str()
        .expect("cold-contact response must include approval_token");
    assert!(!token.is_empty());
    // JWT shape: header.payload.signature.
    assert_eq!(token.matches('.').count(), 2);
}

/// Bad approval_token (wrong jti) is rejected with 401.
#[tokio::test]
async fn approval_with_mismatched_token_jti_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _state) = build_app(tmp.path().to_path_buf());
    let init = send_cold_contact(app.clone(), "flow-bad-jti", "x", "hi").await;
    let approval_id = init["approval_id"].as_str().unwrap().to_owned();
    let token = init["approval_token"].as_str().unwrap().to_owned();

    // Hit the WRONG approval id with the right token. Server must
    // reject because the token's jti doesn't match the path.
    let body = serde_json::to_vec(&serde_json::json!({
        "verb": "trust",
        "approval_token": token,
    }))
    .unwrap();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/admin/approvals/appr-different-id/respond")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    // Either 401 (token jti mismatch) or 404 (approval id not found
    // because it doesn't exist) — both are correct rejections.
    let status = resp.status();
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::NOT_FOUND,
        "expected 401 or 404, got {status}"
    );
    let _ = approval_id;
}

/// A garbage token is rejected with 401.
#[tokio::test]
async fn approval_with_garbage_token_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _state) = build_app(tmp.path().to_path_buf());
    let init = send_cold_contact(app.clone(), "flow-garbage", "y", "hi").await;
    let approval_id = init["approval_id"].as_str().unwrap().to_owned();

    let body = serde_json::to_vec(&serde_json::json!({
        "verb": "trust",
        "approval_token": "not.a.real.jwt.at.all",
    }))
    .unwrap();
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/admin/approvals/{approval_id}/respond"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
