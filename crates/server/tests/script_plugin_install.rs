//! End-to-end smoke test for the script-tier (Rhai) plugin runtime.
//!
//! Builds a tiny `.rhai` plugin in memory, packs it into an install
//! ZIP, posts to `/api/admin/plugins/install`, then drives an
//! `identity.resolve` through the host's dispatch path. The script
//! returns a canned match for one (transport, handle) pair and
//! null for everything else — proves the registry registered the
//! identity_provider hook + the dispatcher routed the call to the
//! script + the script's return JSON round-tripped cleanly.
//!
//! Cross-platform: no shell scripts, no native binaries — the
//! whole runtime is in-process Rust + Rhai.

use axum::body::{self, Body};
use axum::http::{Method, Request, StatusCode, header};
use execlaw_core::db::{Database, DbConfig};
use execlaw_core::migrations::MigrationRunner;
use execlaw_plugin_host::{HookRegistry, PluginHost};
use execlaw_server::{AppState, EventBus, JwtSigner, RefreshStore, ServerConfig};
use std::io::{Cursor, Write};
use std::sync::Arc;
use tower::ServiceExt;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

const PLUGIN_ID: &str = "test-script-id";

const MANIFEST: &str = r#"
[plugin]
id = "test-script-id"
name = "Test Script Plugin"
version = "0.1.0"
description = "Inline-Rhai identity provider for the integration test."
author = "execlaw-test"
license = "AGPL-3.0-or-later"

[identity_provider]
resolves = ["email"]
trust_hint_default = "Contact"
confidence_ceiling = 0.95

[runtime]
tier = "script"
source = "main.rhai"
"#;

const SCRIPT: &str = r#"
fn identity_resolve(transport, handle, oauth) {
    if transport != "email" {
        return #{ "match": () };
    }
    if lower(trim(handle)) == "alice@example.com" {
        return #{ "match": #{
            "stable_principal_id": "pri_test_alice",
            "confidence": 0.95,
            "trust_hint": "Contact",
            "display_name": "Alice",
            "tags": ["script-tier-test"],
            "resolved_by": "test-script-id",
        }};
    }
    #{ "match": () }
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
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    let events = EventBus::new();
    let state = AppState {
        db: db.clone(),
        config: Arc::new(ServerConfig::default()),
        signer: Arc::new(JwtSigner::generate("execlaw-test".into())),
        refresh_store: Arc::new(RefreshStore::new(db.clone())),
        events: events.clone(),
        event_log_hmac_key: Some(Arc::new(b"execlaw-test-hmac-key-32-bytes!!".to_vec())),
        inference: Arc::new(
            execlaw_server::inference_resolver::InferenceResolver::new(None),
        ),
        plugin_host: PluginHost::with_script_engine(
            db.clone(),
            HookRegistry::new(),
            stage_root,
            execlaw_script::ScriptEngine::with_loopback_allowed_for_tests(),
        ),
        webauthn: None,
        mcp_host: execlaw_server::mcp_host::McpHost::new(db),
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
    };
    (
        execlaw_server::routes::build_router(state.clone()),
        state,
    )
}

async fn post_zip(app: axum::Router, bytes: Vec<u8>) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/admin/plugins/install")
        .header(header::CONTENT_TYPE, "application/zip")
        .body(Body::from(bytes))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body_bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn script_plugin_installs_and_resolves_identity() {
    let stage_dir = tempfile::tempdir().unwrap();
    let (app, state) = build_app(stage_dir.path().to_path_buf());

    // Build the install ZIP: manifest + the .rhai script.
    let zip = build_zip(&[
        ("plugin.toml", MANIFEST.as_bytes()),
        ("main.rhai", SCRIPT.as_bytes()),
    ]);
    let (status, body) = post_zip(app, zip).await;
    assert_eq!(status, StatusCode::OK, "install body: {body}");
    assert_eq!(body["plugin_id"], PLUGIN_ID);

    // The hook registry should now show our identity provider.
    let providers = state.plugin_host.registry().identity_providers();
    assert!(
        providers.iter().any(|p| p.plugin_id == PLUGIN_ID),
        "identity_provider hook not registered: {providers:?}"
    );

    // Drive identity.resolve. Match case: known email lowercased.
    let matches = state
        .plugin_host
        .resolve_identity("email", "ALICE@Example.COM")
        .await;
    assert_eq!(matches.len(), 1, "expected 1 match, got {matches:?}");
    assert_eq!(matches[0]["stable_principal_id"], "pri_test_alice");
    assert_eq!(matches[0]["display_name"], "Alice");
    assert_eq!(matches[0]["resolved_by"], PLUGIN_ID);
    assert_eq!(matches[0]["trust_hint"], "Contact");

    // Non-match case: unknown email → no match.
    let none = state
        .plugin_host
        .resolve_identity("email", "stranger@example.com")
        .await;
    assert!(none.is_empty(), "expected no matches, got {none:?}");

    // Wrong transport: phone is not in `resolves` → still no match.
    let wrong_transport = state
        .plugin_host
        .resolve_identity("phone", "alice@example.com")
        .await;
    assert!(
        wrong_transport.is_empty(),
        "phone transport should return no matches: {wrong_transport:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_rejects_script_plugin_with_unparseable_rhai() {
    let stage_dir = tempfile::tempdir().unwrap();
    let (app, _state) = build_app(stage_dir.path().to_path_buf());
    let bad_script = r#"this is not valid rhai!!! syntax error here"#;
    let zip = build_zip(&[
        ("plugin.toml", MANIFEST.as_bytes()),
        ("main.rhai", bad_script.as_bytes()),
    ]);
    let (status, body) = post_zip(app, zip).await;
    // The install endpoint surfaces script-load failures as 4xx /
    // 5xx (whichever the host's error mapping picks). The
    // important thing is it does NOT return 200 — a half-installed
    // plugin would leak hooks.
    assert_ne!(
        status,
        StatusCode::OK,
        "expected install to fail on parse error; body: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_rejects_script_manifest_without_source_file_in_zip() {
    // Manifest declares `source = "main.rhai"` but the ZIP doesn't
    // include it. The script-load step should fail at install time.
    let stage_dir = tempfile::tempdir().unwrap();
    let (app, _state) = build_app(stage_dir.path().to_path_buf());
    let zip = build_zip(&[("plugin.toml", MANIFEST.as_bytes())]);
    let (status, body) = post_zip(app, zip).await;
    assert_ne!(
        status,
        StatusCode::OK,
        "install must fail when manifest source path is missing; body: {body}"
    );
}
