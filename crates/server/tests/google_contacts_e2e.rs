//! End-to-end integration test for the real `plugins/google-contacts`
//! Rhai plugin loaded through the full server install path.
//!
//! What this exercises (which the script-crate test in
//! `crates/script/tests/google_contacts.rs` does NOT):
//!
//!   * The plugin's `plugin.toml` is parsed by the manifest layer
//!     (validates [[oauth_accounts]] + [identity_provider] + [[tools]]).
//!   * The install ZIP gets staged + the script gets compiled at
//!     install time inside `PluginHost::install`.
//!   * The HookRegistry actually picks up the identity-provider and
//!     `contacts.list` tool entries.
//!   * `PluginHost::resolve_identity` reads `state_oauth_tokens` and
//!     hands them to the Rhai script via `oauth_tokens_for`.
//!   * `PluginHost::call_tool` does the same for tool dispatch.
//!
//! Cross-platform: pure in-process Rust + an in-thread mock TCP
//! server for the People API.

use axum::body::{self, Body};
use axum::http::{Method, Request, StatusCode, header};
use execlaw_core::db::{Database, DbConfig};
use execlaw_core::migrations::MigrationRunner;
use execlaw_core::oauth::{OauthClient, OauthClientStore, OauthTokenStore, OauthTokens};
use execlaw_plugin_host::{HookRegistry, PluginHost};
use execlaw_server::{AppState, EventBus, JwtSigner, RefreshStore, ServerConfig};
use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

const PLUGIN_ID: &str = "google-contacts";

const SAMPLE_PEOPLE_RESPONSE: &str = r#"{
  "connections": [
    {
      "resourceName": "people/c12345",
      "names": [{"displayName": "Alice Smith"}],
      "emailAddresses": [
        {"value": "alice@example.com"},
        {"value": "alice.smith@work.example.com"}
      ],
      "phoneNumbers": [
        {"value": "+1 (555) 123-4567"}
      ]
    },
    {
      "resourceName": "people/c67890",
      "names": [{"displayName": "Bob Jones"}],
      "emailAddresses": [{"value": "BOB@example.com"}],
      "phoneNumbers": []
    }
  ]
}"#;

/// Spawn a one-shot mock People API on a random localhost port.
/// Returns the bound `http://127.0.0.1:PORT` URL. Runs until the
/// listener is dropped at process exit.
fn spawn_mock_people_api(body: &'static str) -> (String, Arc<Mutex<u32>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(Mutex::new(0u32));
    let calls_w = calls.clone();
    std::thread::spawn(move || {
        loop {
            let Ok((mut sock, _)) = listener.accept() else {
                break;
            };
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf);
            *calls_w.lock().unwrap() += 1;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
        }
    });
    (format!("http://{addr}"), calls)
}

/// Read `plugins/google-contacts/{plugin.toml,main.rhai}` from the
/// workspace root, rewriting the People API URL in the script to
/// point at the mock server.
fn load_plugin_files(mock_url: &str) -> (String, String) {
    // CARGO_MANIFEST_DIR is crates/server. Walk up to the workspace root.
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let manifest =
        std::fs::read_to_string(workspace_root.join("plugins/google-contacts/plugin.toml"))
            .expect("plugins/google-contacts/plugin.toml must exist");
    let original_script =
        std::fs::read_to_string(workspace_root.join("plugins/google-contacts/main.rhai"))
            .expect("plugins/google-contacts/main.rhai must exist");
    let rewritten = original_script.replace(
        r#""https://people.googleapis.com/v1/people/me/connections""#,
        &format!(r#""{mock_url}/v1/people/me/connections""#),
    );
    (manifest, rewritten)
}

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

fn build_app(stage_root: PathBuf) -> (axum::Router, AppState) {
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
        sidecar_supervisor: None,
        host_transports: execlaw_server::transport_registry::HostTransportRegistry::new(),
        skill_capture: execlaw_skills::AutoCaptureSink::noop(),
        reuse_update: execlaw_skills::ReuseUpdateSink::noop(),
    };
    (execlaw_server::routes::build_router(state.clone()), state)
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

async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body_bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

/// Seed state_oauth_clients (required by FK) + state_oauth_tokens
/// for (google-contacts, controller) with the given access token.
fn seed_oauth(db: &Database, access_token: &str) {
    let cs = OauthClientStore::new(db);
    cs.upsert(&OauthClient {
        plugin_id: PLUGIN_ID.into(),
        account_name: "controller".into(),
        provider: "google".into(),
        client_id: "test-client.apps.googleusercontent.com".into(),
        client_secret: "GOCSPX-test-secret".into(),
        redirect_uri: "http://localhost:3030/api/oauth/google/callback".into(),
        scopes_json: serde_json::to_string(&vec![
            "https://www.googleapis.com/auth/contacts.readonly".to_owned(),
        ])
        .unwrap(),
        created_at: 100,
        updated_at: 100,
    })
    .unwrap();
    let ts = OauthTokenStore::new(db);
    ts.upsert(&OauthTokens {
        plugin_id: PLUGIN_ID.into(),
        account_name: "controller".into(),
        access_token: access_token.into(),
        refresh_token: Some("1//refresh-test".into()),
        token_expires_at: 9_999_999_999,
        scopes_granted: serde_json::to_string(&vec![
            "https://www.googleapis.com/auth/contacts.readonly".to_owned(),
        ])
        .unwrap(),
        account_email: Some("operator@example.com".into()),
        created_at: 100,
        updated_at: 100,
    })
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn google_contacts_plugin_full_install_and_dispatch_roundtrip() {
    let (mock_url, _calls) = spawn_mock_people_api(SAMPLE_PEOPLE_RESPONSE);
    let (manifest, script) = load_plugin_files(&mock_url);

    let stage_dir = tempfile::tempdir().unwrap();
    let (app, state) = build_app(stage_dir.path().to_path_buf());

    // ---- 1. Install via HTTP -------------------------------------
    let zip = build_zip(&[
        ("plugin.toml", manifest.as_bytes()),
        ("main.rhai", script.as_bytes()),
    ]);
    let (status, body) = post_zip(app.clone(), zip).await;
    assert_eq!(status, StatusCode::OK, "install body: {body}");
    assert_eq!(body["plugin_id"], PLUGIN_ID);

    // ---- 2. /api/admin/plugins shows google-contacts -------------
    let (status, body) = get_json(app.clone(), "/api/admin/plugins").await;
    assert_eq!(status, StatusCode::OK);
    let plugins = body["plugins"].as_array().expect("plugins[] in response");
    assert!(
        plugins.iter().any(|p| p["plugin_id"] == PLUGIN_ID),
        "google-contacts not in /api/admin/plugins: {body}"
    );

    // ---- 3. /api/admin/plugins/tools shows contacts.list ---------
    let (status, body) = get_json(app.clone(), "/api/admin/plugins/tools").await;
    assert_eq!(status, StatusCode::OK);
    let tools = body["tools"].as_array().expect("tools[] in response");
    assert!(
        tools
            .iter()
            .any(|t| t["name"] == "contacts.list" && t["plugin_id"] == PLUGIN_ID),
        "contacts.list not in /api/admin/plugins/tools: {body}"
    );

    // ---- 4. resolve_identity WITH oauth token --------------------
    seed_oauth(&state.db, "ya29.fake");
    let matches = state
        .plugin_host
        .resolve_identity("email", "ALICE@Example.COM")
        .await;
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one match with token seeded; got {matches:?}"
    );
    let m = &matches[0];
    assert_eq!(m["display_name"], "Alice Smith");
    assert_eq!(m["resolved_by"], PLUGIN_ID);
    assert_eq!(m["trust_hint"], "Contact");
    let id = m["stable_principal_id"].as_str().unwrap();
    assert!(
        id.starts_with("pri_google_"),
        "expected pri_google_ prefix, got {id}"
    );

    // ---- 5. call_tool contacts.list with token -------------------
    let res = state
        .plugin_host
        .call_tool("contacts.list", serde_json::json!({"limit": 5}), &["*"], None)
        .await
        .expect("contacts.list dispatch should succeed with valid token + caps");
    let contacts = res["contacts"]
        .as_array()
        .expect("response should have contacts[] array");
    assert_eq!(contacts.len(), 2, "mock returns 2 contacts: {res}");
    assert_eq!(res["total"], 2);
    assert_eq!(res["limit"], 5);
    let alice = contacts
        .iter()
        .find(|c| c["display_name"] == "Alice Smith")
        .expect("Alice should be in contacts");
    assert_eq!(alice["emails"].as_array().unwrap().len(), 2);

    // ---- 6. resolve_identity WITHOUT oauth token (token deleted) -
    let removed = OauthTokenStore::new(&state.db)
        .delete(PLUGIN_ID, "controller")
        .unwrap();
    assert!(removed, "token row should have been removed");
    let none = state
        .plugin_host
        .resolve_identity("email", "alice@example.com")
        .await;
    assert!(
        none.is_empty(),
        "with no oauth token persisted, identity provider should yield no matches; got {none:?}"
    );
}
