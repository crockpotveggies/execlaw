//! Integration tests for host-enforced webhook authentication.
//!
//! Covers the security regression from the 2026-05 audit: webhook
//! POSTs were being persisted to the automation bus BEFORE the
//! plugin's Rhai handler had a chance to validate the caller. The
//! fix moves auth verification to the host, runs it immediately
//! after route lookup, and rejects unauthenticated hits with 401
//! before any side effect (bus publish, body decode, handler call).
//!
//! Each test asserts the two halves of the contract:
//!   1. Wire-level: 401 vs 200 on the HTTP response.
//!   2. Durable: the `state_bus_events` table is or isn't populated.
//!
//! These are the regression tests the audit specifically asked for â€”
//! "add tests proving invalid webhook tokens do not create bus
//! events or automation runs."

use axum::body::{self, Body};
use axum::http::{Method, Request, StatusCode, header};
use execlaw_core::automation_bus::{BusEventKind, BusEventStore};
use execlaw_core::db::{Database, DbConfig};
use execlaw_core::migrations::MigrationRunner;
use execlaw_core::vault_row::VaultRowStore;
use execlaw_plugin_host::{HookRegistry, PluginHost};
use execlaw_server::{AppState, EventBus, JwtSigner, RefreshStore, ServerConfig};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::io::{Cursor, Write};
use std::sync::Arc;
use tower::ServiceExt;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

const PLUGIN_ID: &str = "wh-test";
const WEBHOOK_SECRET: &str = "s3cr3t-shared-with-third-party";

/// Plugin that declares a single host-authenticated webhook route.
/// The Rhai handler simply echoes back a fixed `{"ok": true}` so a
/// 200 response means "host auth passed AND handler ran."
const MANIFEST_QUERY_TOKEN: &str = r#"
[plugin]
id = "wh-test"
name = "Webhook Auth Test"
version = "0.1.0"

[[webhook_routes]]
method = "POST"
path = "/event"
handler = "on_event"
description = "Test webhook with host-enforced query-token auth."
auth = { kind = "query_token", query = "token", vault_key = "webhook_secret" }

[runtime]
tier = "script"
source = "main.rhai"
"#;

/// Plugin variant using HMAC-SHA256 header auth (GitHub-style).
const MANIFEST_HMAC: &str = r#"
[plugin]
id = "wh-test"
name = "Webhook Auth Test"
version = "0.1.0"

[[webhook_routes]]
method = "POST"
path = "/event"
handler = "on_event"
description = "Test webhook with host-enforced HMAC-SHA256 header auth."
auth = { kind = "hmac_sha256_header", header = "X-Signature", vault_key = "webhook_secret" }

[runtime]
tier = "script"
source = "main.rhai"
"#;

/// Legacy plugin â€” no `auth` field. Asserts the dispatcher preserves
/// today's behavior (handler validates) for backward compatibility.
const MANIFEST_LEGACY: &str = r#"
[plugin]
id = "wh-test"
name = "Webhook Auth Test"
version = "0.1.0"

[[webhook_routes]]
method = "POST"
path = "/event"
handler = "on_event"
description = "Legacy webhook with no host-level auth declaration."

[runtime]
tier = "script"
source = "main.rhai"
"#;

const SCRIPT: &str = r#"
fn on_event(args) {
    // Handler doesn't re-check auth â€” the test is asserting the
    // host's check. A 200 from this means the host let us through.
    #{ "ok": true }
}
"#;

fn build_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut zw = ZipWriter::new(&mut buf);
        let opts = SimpleFileOptions::default();
        for (name, bytes) in files {
            zw.start_file::<_, ()>(*name, opts).unwrap();
            zw.write_all(bytes).unwrap();
        }
        zw.finish().unwrap();
    }
    buf.into_inner()
}

fn build_app(stage_root: std::path::PathBuf) -> (axum::Router, AppState) {
    let db_config = DbConfig::in_memory_unencrypted();
    let db = Database::open(&db_config).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    let events = EventBus::new();
    let state = AppState {
        db: db.clone(),
        db_config: Arc::new(db_config),
        config: Arc::new(ServerConfig::default()),
        signer: Arc::new(JwtSigner::generate("execlaw-test".into())),
        refresh_store: Arc::new(RefreshStore::new(db.clone())),
        events: events.clone(),
        event_log_hmac_key: Some(Arc::new(b"execlaw-test-hmac-key-32-bytes!!".to_vec())),
        inference: Arc::new(execlaw_server::inference_resolver::InferenceResolver::new(
            None,
        )),
        plugin_host: PluginHost::with_script_engine(
            db.clone(),
            HookRegistry::new(),
            stage_root,
            execlaw_script::ScriptEngine::with_loopback_allowed_for_tests(),
        ),
        webauthn: None,
        mcp_host: execlaw_server::mcp_host::McpHost::new(db.clone()),
        backend_supervisor: None,
        voice_sessions: execlaw_server::voice_session::VoiceSessionRegistry::new(events.clone()),
        voice_runtime: execlaw_server::voice_runtime::VoiceRuntime::new(
            events,
            Arc::new(|| {
                Box::new(execlaw_voice_pipeline::traits::MockStt::new(
                    Vec::new(),
                    String::new(),
                ))
            }),
            Arc::new(|| {
                (
                    Box::new(execlaw_voice_pipeline::traits::MockTts::default())
                        as Box<dyn execlaw_voice_pipeline::traits::TtsClient>,
                    None,
                )
            }),
        ),
        turn_cancel: execlaw_server::turn_cancel::TurnCancellationRegistry::new(),
        runner_supervisor: None,
        research_supervisor: None,
        sidecar_supervisor: None,
        host_transports: execlaw_server::transport_registry::HostTransportRegistry::new(),
        skill_capture: execlaw_skills::AutoCaptureSink::noop(),
        reuse_update: execlaw_skills::ReuseUpdateSink::noop(),
        automation_bus: execlaw_server::automation_bus::AutomationBus::stub(db),
        automation_agent_pool: execlaw_server::automation_agent::AutomationsAgentPool::new(
            std::sync::Arc::new(execlaw_server::automation_agent::StubAgentInvoker::err(
                "test pool: no LLM",
            )),
        ),
        flow_channel: execlaw_server::flow_channel::FlowChannelHub::new(),
        data_dir: std::env::temp_dir().join(format!("execlaw-test-{}", uuid::Uuid::new_v4())),
        inference_metrics: execlaw_server::inference_metrics::InferenceMetrics::new(),
    };
    (execlaw_server::routes::build_router(state.clone()), state)
}

async fn install_plugin(app: axum::Router, manifest: &str) {
    let zip = build_zip(&[
        ("plugin.toml", manifest.as_bytes()),
        ("main.rhai", SCRIPT.as_bytes()),
    ]);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/admin/plugins/install")
                .header(header::CONTENT_TYPE, "application/zip")
                .body(Body::from(zip))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body_bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "install failed: {}",
        String::from_utf8_lossy(&body_bytes)
    );
}

fn seed_secret(state: &AppState, value: &str) {
    let store = VaultRowStore::new(&state.db);
    store
        .put(Some(PLUGIN_ID), "webhook_secret", value.as_bytes(), 0)
        .unwrap();
}

async fn post_webhook(
    app: axum::Router,
    uri: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = app
        .oneshot(req.body(Body::from(body.to_vec())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body_bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, body_bytes.to_vec())
}

fn count_webhook_events(state: &AppState) -> usize {
    BusEventStore::new(&state.db)
        .list_recent_for_kind(BusEventKind::WebhookReceived.as_str(), 100)
        .unwrap()
        .len()
}

/// Critical: missing token â†’ 401, NO bus event, NO handler call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_token_missing_rejects_with_no_bus_event() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, state) = build_app(tmp.path().to_path_buf());
    install_plugin(app.clone(), MANIFEST_QUERY_TOKEN).await;
    seed_secret(&state, WEBHOOK_SECRET);

    let (status, _body) =
        post_webhook(app, "/api/webhooks/wh-test/event", &[], br#"{"hello":1}"#).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        count_webhook_events(&state),
        0,
        "missing token must NOT create a bus event"
    );
}

/// Critical: wrong token â†’ 401, NO bus event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_token_mismatch_rejects_with_no_bus_event() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, state) = build_app(tmp.path().to_path_buf());
    install_plugin(app.clone(), MANIFEST_QUERY_TOKEN).await;
    seed_secret(&state, WEBHOOK_SECRET);

    let (status, _body) = post_webhook(
        app,
        "/api/webhooks/wh-test/event?token=wrong",
        &[],
        br#"{"hello":1}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        count_webhook_events(&state),
        0,
        "wrong token must NOT create a bus event"
    );
}

/// Critical: vault row absent â†’ 401, NO bus event, even if caller
/// supplied an empty `?token=`. A missing secret is not "no auth
/// required" â€” it's a misconfigured plugin and must fail closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_token_with_missing_vault_row_rejects() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, state) = build_app(tmp.path().to_path_buf());
    install_plugin(app.clone(), MANIFEST_QUERY_TOKEN).await;
    // NOTE: deliberately not calling seed_secret â€” the vault row
    // is missing.

    let (status, _body) = post_webhook(
        app,
        "/api/webhooks/wh-test/event?token=anything",
        &[],
        br#"{"hello":1}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(count_webhook_events(&state), 0);
}

/// Happy path: correct token â†’ 200 from handler AND a bus event.
/// Bus event payload must NOT contain the literal secret (the
/// `token` query key is redacted).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_token_valid_accepts_and_redacts_payload() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, state) = build_app(tmp.path().to_path_buf());
    install_plugin(app.clone(), MANIFEST_QUERY_TOKEN).await;
    seed_secret(&state, WEBHOOK_SECRET);

    let uri = format!("/api/webhooks/wh-test/event?token={}", WEBHOOK_SECRET);
    let (status, resp_body) = post_webhook(app, &uri, &[], br#"{"hello":1}"#).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "valid token must reach handler: {}",
        String::from_utf8_lossy(&resp_body)
    );

    let events = BusEventStore::new(&state.db)
        .list_recent_for_kind(BusEventKind::WebhookReceived.as_str(), 10)
        .unwrap();
    assert_eq!(
        events.len(),
        1,
        "valid token must create exactly one bus event"
    );
    let payload = &events[0].payload;
    // Persisted payload's query.token must be redacted.
    assert_eq!(
        payload["query"]["token"], "<redacted>",
        "secret must be redacted in persisted payload"
    );
    let payload_str = serde_json::to_string(payload).unwrap();
    assert!(
        !payload_str.contains(WEBHOOK_SECRET),
        "secret leaked into persisted payload: {payload_str}"
    );
}

/// HMAC-SHA256 header auth: matching signature â†’ 200 + bus event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hmac_header_valid_accepts() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, state) = build_app(tmp.path().to_path_buf());
    install_plugin(app.clone(), MANIFEST_HMAC).await;
    seed_secret(&state, WEBHOOK_SECRET);

    let body = br#"{"event":"ping"}"#.to_vec();
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(WEBHOOK_SECRET.as_bytes()).unwrap();
    mac.update(&body);
    let sig_hex = hex::encode(mac.finalize().into_bytes());

    let (status, _) = post_webhook(
        app,
        "/api/webhooks/wh-test/event",
        &[("X-Signature", &format!("sha256={sig_hex}"))],
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(count_webhook_events(&state), 1);
}

/// HMAC-SHA256 header auth: wrong signature â†’ 401, no bus event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hmac_header_mismatch_rejects() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, state) = build_app(tmp.path().to_path_buf());
    install_plugin(app.clone(), MANIFEST_HMAC).await;
    seed_secret(&state, WEBHOOK_SECRET);

    let body = br#"{"event":"ping"}"#.to_vec();
    // Sign with the wrong secret.
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(b"not-the-secret").unwrap();
    mac.update(&body);
    let sig_hex = hex::encode(mac.finalize().into_bytes());

    let (status, _) = post_webhook(
        app,
        "/api/webhooks/wh-test/event",
        &[("X-Signature", &sig_hex)],
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(count_webhook_events(&state), 0);
}

/// HMAC-SHA256 header auth: signature header absent entirely â†’ 401.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hmac_header_missing_rejects() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, state) = build_app(tmp.path().to_path_buf());
    install_plugin(app.clone(), MANIFEST_HMAC).await;
    seed_secret(&state, WEBHOOK_SECRET);

    let (status, _) = post_webhook(
        app,
        "/api/webhooks/wh-test/event",
        &[],
        br#"{"event":"ping"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(count_webhook_events(&state), 0);
}

/// Backward compatibility: a plugin manifest with NO `auth` field
/// still works â€” the dispatcher falls back to the legacy "handler
/// validates" model and the bus event IS published. This preserves
/// today's behavior for any external plugin not yet migrated to
/// host-enforced auth. The deprecation warning is logged (we don't
/// assert on logs, but the dispatcher does emit it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_no_auth_field_still_dispatches() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, state) = build_app(tmp.path().to_path_buf());
    install_plugin(app.clone(), MANIFEST_LEGACY).await;

    let (status, _) = post_webhook(
        app,
        "/api/webhooks/wh-test/event",
        &[],
        br#"{"event":"ping"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "manifest without `auth` must still dispatch (legacy compat)"
    );
    assert_eq!(
        count_webhook_events(&state),
        1,
        "legacy mode still publishes a bus event"
    );
}

/// Legacy mode also redacts common secret query keys from the
/// persisted bus payload, even though the host doesn't enforce
/// auth on that route. Defense-in-depth: if an external plugin's
/// handler validates a `?token=` query param the way the in-tree
/// WhatsApp plugin used to, the literal secret never lands in the
/// durable bus log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_mode_still_redacts_known_secret_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, state) = build_app(tmp.path().to_path_buf());
    install_plugin(app.clone(), MANIFEST_LEGACY).await;

    let (status, _) = post_webhook(
        app,
        "/api/webhooks/wh-test/event?token=should-be-redacted&user=alice",
        &[],
        br#"{}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = BusEventStore::new(&state.db)
        .list_recent_for_kind(BusEventKind::WebhookReceived.as_str(), 10)
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload["query"]["token"], "<redacted>");
    assert_eq!(events[0].payload["query"]["user"], "alice");
}
