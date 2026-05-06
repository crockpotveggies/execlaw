//! End-to-end integration test for the real `plugins/google-calendar`
//! Rhai plugin loaded through the full server install path.
//!
//! Same shape as `google_contacts_e2e.rs`. Exercises:
//!
//!   * The plugin's `plugin.toml` parses (validates [[oauth_accounts]]
//!     + two [[tools]]).
//!   * The install ZIP gets staged + the script gets compiled at
//!     install time inside `PluginHost::install`.
//!   * The HookRegistry picks up `calendar.list_calendars` +
//!     `calendar.list_events` tool entries.
//!   * `PluginHost::call_tool` reads `state_oauth_tokens` and hands
//!     them to the Rhai script via `oauth_tokens_for`.
//!   * Both tools dispatch through end-to-end against a local mock
//!     Calendar API.
//!
//! Cross-platform: pure in-process Rust + an in-thread mock TCP
//! server.

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

const PLUGIN_ID: &str = "google-calendar";

const SAMPLE_CALENDARS: &str = r#"{
  "items": [
    {
      "id": "primary",
      "summary": "alice@example.com",
      "timeZone": "America/Los_Angeles",
      "primary": true,
      "accessRole": "owner"
    },
    {
      "id": "team-shared@group.calendar.google.com",
      "summary": "Team Shared",
      "timeZone": "UTC",
      "accessRole": "reader"
    }
  ]
}"#;

const SAMPLE_EVENTS: &str = r#"{
  "items": [
    {
      "id": "evt-1",
      "summary": "Standup",
      "location": "Zoom",
      "status": "confirmed",
      "start": {"dateTime": "2026-05-02T15:00:00Z", "timeZone": "UTC"},
      "end":   {"dateTime": "2026-05-02T15:30:00Z", "timeZone": "UTC"}
    },
    {
      "id": "evt-2",
      "summary": "Holiday",
      "status": "confirmed",
      "start": {"date": "2026-05-04"},
      "end":   {"date": "2026-05-05"}
    }
  ]
}"#;

/// Mock Calendar API. Routes by URL path prefix:
///   /calendar/v3/users/me/calendarList → SAMPLE_CALENDARS
///   /calendar/v3/calendars/<id>/events → SAMPLE_EVENTS
fn spawn_mock_calendar_api() -> (String, Arc<Mutex<u32>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(Mutex::new(0u32));
    let calls_w = calls.clone();
    std::thread::spawn(move || {
        loop {
            let Ok((mut sock, _)) = listener.accept() else {
                break;
            };
            let mut buf = [0u8; 16384];
            let n = sock.read(&mut buf).unwrap_or(0);
            *calls_w.lock().unwrap() += 1;
            let req = std::str::from_utf8(&buf[..n]).unwrap_or("");
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/");
            let body: &str = if path.starts_with("/calendar/v3/users/me/calendarList") {
                SAMPLE_CALENDARS
            } else if path.contains("/events") {
                SAMPLE_EVENTS
            } else {
                r#"{"items":[]}"#
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
        }
    });
    (format!("http://{addr}"), calls)
}

/// Read `plugins/google-calendar/{plugin.toml,main.rhai}` from the
/// workspace root, rewriting both Calendar API URLs in the script
/// to point at the mock server.
fn load_plugin_files(mock_url: &str) -> (String, String) {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let manifest =
        std::fs::read_to_string(workspace_root.join("plugins/google-calendar/plugin.toml"))
            .expect("plugins/google-calendar/plugin.toml must exist");
    let original_script =
        std::fs::read_to_string(workspace_root.join("plugins/google-calendar/main.rhai"))
            .expect("plugins/google-calendar/main.rhai must exist");
    let rewritten = original_script
        .replace(
            r#""https://www.googleapis.com/calendar/v3/users/me/calendarList""#,
            &format!(r#""{mock_url}/calendar/v3/users/me/calendarList""#),
        )
        .replace(
            r#""https://www.googleapis.com/calendar/v3/calendars/""#,
            &format!(r#""{mock_url}/calendar/v3/calendars/""#),
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

fn seed_oauth(db: &Database, token: &str) {
    let now = chrono::Utc::now().timestamp();
    OauthClientStore::new(db)
        .upsert(&OauthClient {
            plugin_id: PLUGIN_ID.into(),
            account_name: "controller".into(),
            provider: "google".into(),
            client_id: "fake.apps.googleusercontent.com".into(),
            client_secret: "fake-secret".into(),
            redirect_uri: "http://localhost:3030/api/oauth/google/callback".into(),
            scopes_json: serde_json::to_string(&vec![
                "https://www.googleapis.com/auth/calendar.readonly".to_owned(),
            ])
            .unwrap(),
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    OauthTokenStore::new(db)
        .upsert(&OauthTokens {
            plugin_id: PLUGIN_ID.into(),
            account_name: "controller".into(),
            access_token: token.into(),
            refresh_token: Some("fake-refresh".into()),
            token_expires_at: now + 3600,
            scopes_granted: serde_json::to_string(&vec![
                "https://www.googleapis.com/auth/calendar.readonly".to_owned(),
            ])
            .unwrap(),
            account_email: Some("alice@example.com".into()),
            created_at: now,
            updated_at: now,
        })
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn google_calendar_plugin_full_install_and_dispatch_roundtrip() {
    let (mock_url, _calls) = spawn_mock_calendar_api();
    let stage_dir = tempfile::tempdir().unwrap();
    let (app, state) = build_app(stage_dir.path().to_path_buf());

    // ---- 1. Install the plugin via the production HTTP path -----
    let (manifest, script) = load_plugin_files(&mock_url);
    let zip = build_zip(&[
        ("plugin.toml", manifest.as_bytes()),
        ("main.rhai", script.as_bytes()),
    ]);
    let (status, body) = post_zip(app.clone(), zip).await;
    assert_eq!(status, StatusCode::OK, "install body: {body}");
    assert_eq!(body["plugin_id"], PLUGIN_ID);

    // ---- 2. Plugin appears in /api/admin/plugins ----------------
    let (status, body) = get_json(app.clone(), "/api/admin/plugins").await;
    assert_eq!(status, StatusCode::OK);
    let plugins = body["plugins"].as_array().expect("plugins[] in response");
    let row = plugins
        .iter()
        .find(|p| p["plugin_id"] == PLUGIN_ID)
        .unwrap_or_else(|| panic!("google-calendar not in /api/admin/plugins: {body}"));
    // Has [[oauth_accounts]] declared → has_settings_ui = true.
    assert_eq!(row["has_settings_ui"], true);

    // ---- 3. Both tools land in /api/admin/plugins/tools ---------
    let (status, body) = get_json(app.clone(), "/api/admin/plugins/tools").await;
    assert_eq!(status, StatusCode::OK);
    let tools = body["tools"].as_array().expect("tools[] in response");
    for tool_name in ["calendar.list_calendars", "calendar.list_events"] {
        assert!(
            tools
                .iter()
                .any(|t| t["name"] == tool_name && t["plugin_id"] == PLUGIN_ID),
            "{tool_name} not in /api/admin/plugins/tools: {body}"
        );
    }

    // ---- 4. call_tool calendar.list_calendars without token ----
    // Returns Err since the plugin's tool_call throws on missing token.
    let err = state
        .plugin_host
        .call_tool("calendar.list_calendars", serde_json::json!({}), &["*"], None)
        .await
        .unwrap_err();
    assert!(
        err.contains("not connected"),
        "expected 'not connected' error before token seeded; got: {err}"
    );

    // ---- 5. Seed OAuth + call_tool calendar.list_calendars ------
    seed_oauth(&state.db, "ya29.fake");
    let res = state
        .plugin_host
        .call_tool("calendar.list_calendars", serde_json::json!({}), &["*"], None)
        .await
        .expect("calendar.list_calendars should succeed with token + caps");
    let cals = res["calendars"].as_array().unwrap();
    assert_eq!(cals.len(), 2);
    assert_eq!(cals[0]["id"], "primary");
    assert_eq!(cals[0]["primary"], true);

    // ---- 6. call_tool calendar.list_events default args ---------
    let res = state
        .plugin_host
        .call_tool("calendar.list_events", serde_json::json!({}), &["*"], None)
        .await
        .expect("calendar.list_events should succeed");
    let events = res["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["summary"], "Standup");
    assert_eq!(events[0]["start"]["kind"], "timed");
    assert_eq!(events[1]["summary"], "Holiday");
    assert_eq!(events[1]["start"]["kind"], "all_day");
    assert_eq!(res["calendar_id"], "primary");
}
