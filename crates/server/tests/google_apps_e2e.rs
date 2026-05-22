//! End-to-end integration test for the real `plugins/google-apps`
//! Rhai plugin loaded through the full server install path.
//!
//! Exercises:
//!   * `plugin.toml` parses (validates [[oauth_accounts]],
//!     [identity_provider], and tools across all five modules).
//!   * Install ZIP gets staged + compiled at install time.
//!   * HookRegistry picks up tool entries from each module.
//!   * `PluginHost::call_tool` dispatches against a mock API.
//!   * `enabled_modules` toggle gating works through the full vault
//!     â†’ script path (write the setting, observe disabled tools
//!     throw a clear error).
//!   * Trust-floor adversarial: a sub-Controller caller invoking
//!     `gmail.send_message` is rejected before the script runs.
//!
//! Cross-platform: pure in-process Rust + in-thread mock TCP server.

use axum::body::{self, Body};
use axum::http::{Method, Request, StatusCode, header};
use execlaw_core::db::{Database, DbConfig};
use execlaw_core::migrations::MigrationRunner;
use execlaw_core::oauth::{OauthClient, OauthClientStore, OauthTokenStore, OauthTokens};
use execlaw_core::vault_row::VaultRowStore;
use execlaw_plugin_host::{HookRegistry, PluginHost};
use execlaw_server::{AppState, EventBus, JwtSigner, RefreshStore, ServerConfig};
use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

const PLUGIN_ID: &str = "google-apps";

/// Mock Google API. Routes by URL path prefix and returns canned
/// JSON for the endpoints `google-apps` reaches in this test.
fn spawn_mock_api() -> (String, Arc<Mutex<u32>>) {
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
            let bare = path.split('?').next().unwrap_or(path);
            let body: &str = if bare.starts_with("/gmail/v1/users/me/labels") {
                r#"{"labels":[{"id":"INBOX","name":"INBOX","type":"system"}]}"#
            } else if bare.starts_with("/calendar/v3/users/me/calendarList") {
                r#"{"items":[{"id":"primary","summary":"a","primary":true,"accessRole":"owner","timeZone":"UTC"}]}"#
            } else if bare.starts_with("/v1/people/me/connections") {
                r#"{"connections":[{"resourceName":"people/c1","names":[{"displayName":"Alice"}],"emailAddresses":[{"value":"a@x"}]}]}"#
            } else if bare.starts_with("/tasks/v1/users/@me/lists") {
                r#"{"items":[{"id":"l1","title":"My Tasks"}]}"#
            } else if bare.starts_with("/drive/v3/files") {
                r#"{"files":[{"id":"f1","name":"x.pdf","mimeType":"application/pdf"}]}"#
            } else {
                r#"{}"#
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

fn load_plugin_files(mock_url: &str) -> (String, String) {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let manifest = std::fs::read_to_string(workspace_root.join("plugins/google-apps/plugin.toml"))
        .expect("plugins/google-apps/plugin.toml must exist");
    let original_script =
        std::fs::read_to_string(workspace_root.join("plugins/google-apps/main.rhai"))
            .expect("plugins/google-apps/main.rhai must exist");
    let rewritten = original_script
        .replace(
            r#""https://gmail.googleapis.com/gmail/v1/users/me/labels""#,
            &format!(r#""{mock_url}/gmail/v1/users/me/labels""#),
        )
        .replace(
            r#""https://www.googleapis.com/calendar/v3/users/me/calendarList""#,
            &format!(r#""{mock_url}/calendar/v3/users/me/calendarList""#),
        )
        .replace(
            r#""https://people.googleapis.com/v1/people/me/connections""#,
            &format!(r#""{mock_url}/v1/people/me/connections""#),
        )
        .replace(
            r#""https://tasks.googleapis.com/tasks/v1/users/@me/lists""#,
            &format!(r#""{mock_url}/tasks/v1/users/@me/lists""#),
        )
        .replace(
            r#""https://www.googleapis.com/drive/v3/files""#,
            &format!(r#""{mock_url}/drive/v3/files""#),
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

        data_dir: std::env::temp_dir().join(format!("execlaw-test-{}", uuid::Uuid::new_v4())),
        inference_metrics: execlaw_server::inference_metrics::InferenceMetrics::new(),
    };
    // Wire host capabilities into the script engine â€” without this
    // the Rhai script's vault_get / vault_put / sidecar_url all
    // error with "host capabilities not wired". Production does
    // this in cli/main.rs after constructing AppState; tests have
    // to replicate it explicitly.
    let caps =
        execlaw_server::host_caps_impl::AppStateHostCapabilities::new(state.clone()).into_arc();
    let _ = state.plugin_host.attach_host_capabilities(caps);
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
            redirect_uri: "http://localhost:3031/api/oauth/google/callback".into(),
            scopes_json: serde_json::to_string(&vec!["openid".to_owned()]).unwrap(),
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
            scopes_granted: serde_json::to_string(&vec!["openid".to_owned()]).unwrap(),
            account_email: Some("alice@example.com".into()),
            created_at: now,
            updated_at: now,
        })
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn google_apps_plugin_installs_and_registers_all_five_modules() {
    let (mock_url, _calls) = spawn_mock_api();
    let stage_dir = tempfile::tempdir().unwrap();
    let (app, state) = build_app(stage_dir.path().to_path_buf());

    // ---- Install ------------------------------------------------
    let (manifest, script) = load_plugin_files(&mock_url);
    let zip = build_zip(&[
        ("plugin.toml", manifest.as_bytes()),
        ("main.rhai", script.as_bytes()),
    ]);
    let (status, body) = post_zip(app.clone(), zip).await;
    assert_eq!(status, StatusCode::OK, "install body: {body}");
    assert_eq!(body["plugin_id"], PLUGIN_ID);

    // ---- Plugin appears in catalog ------------------------------
    let (status, body) = get_json(app.clone(), "/api/admin/plugins").await;
    assert_eq!(status, StatusCode::OK);
    let plugins = body["plugins"].as_array().expect("plugins[] in response");
    let row = plugins
        .iter()
        .find(|p| p["plugin_id"] == PLUGIN_ID)
        .unwrap_or_else(|| panic!("google-apps not in /api/admin/plugins: {body}"));
    assert_eq!(row["has_settings_ui"], true);

    // ---- Every module's representative tool is registered -------
    let (status, body) = get_json(app.clone(), "/api/admin/plugins/tools").await;
    assert_eq!(status, StatusCode::OK);
    let tools = body["tools"].as_array().expect("tools[] in response");
    for tool_name in [
        "gmail.list_labels",
        "calendar.list_calendars",
        "contacts.list",
        "tasks.list_lists",
        "drive.search",
        // And the destructive Gmail tool we'll adversarially exercise:
        "gmail.send_message",
    ] {
        assert!(
            tools
                .iter()
                .any(|t| t["name"] == tool_name && t["plugin_id"] == PLUGIN_ID),
            "{tool_name} not in /api/admin/plugins/tools",
        );
    }

    // ---- Seed OAuth + dispatch a tool from each module ----------
    // Every google-apps tool except calendar.check_availability now
    // pins Controller, so the caller must be Controller-trust to
    // reach the script. Sub-Controller adversarial coverage lives in
    // `controller_floor_tool_rejects_sub_controller_caller` below;
    // here we just want to verify dispatch wiring works end-to-end.
    seed_oauth(&state.db, "ya29.fake");
    // Gmail
    let res = state
        .plugin_host
        .call_tool(
            "gmail.list_labels",
            serde_json::json!({}),
            &["*"],
            Some("Controller"),
        )
        .await
        .expect("gmail.list_labels should succeed");
    assert!(res["labels"].as_array().is_some());
    // Calendar
    let res = state
        .plugin_host
        .call_tool(
            "calendar.list_calendars",
            serde_json::json!({}),
            &["*"],
            Some("Controller"),
        )
        .await
        .expect("calendar.list_calendars should succeed");
    assert!(res["calendars"].as_array().is_some());
    // Contacts
    let res = state
        .plugin_host
        .call_tool(
            "contacts.list",
            serde_json::json!({}),
            &["*"],
            Some("Controller"),
        )
        .await
        .expect("contacts.list should succeed");
    assert!(res["contacts"].as_array().is_some());
    // Tasks
    let res = state
        .plugin_host
        .call_tool(
            "tasks.list_lists",
            serde_json::json!({}),
            &["*"],
            Some("Controller"),
        )
        .await
        .expect("tasks.list_lists should succeed");
    assert!(res["task_lists"].as_array().is_some());
    // Drive
    let res = state
        .plugin_host
        .call_tool(
            "drive.search",
            serde_json::json!({"query": "anything"}),
            &["*"],
            Some("Controller"),
        )
        .await
        .expect("drive.search should succeed");
    assert!(res["files"].as_array().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enabled_modules_setting_gates_dispatch() {
    let (mock_url, _calls) = spawn_mock_api();
    let stage_dir = tempfile::tempdir().unwrap();
    let (app, state) = build_app(stage_dir.path().to_path_buf());

    let (manifest, script) = load_plugin_files(&mock_url);
    let zip = build_zip(&[
        ("plugin.toml", manifest.as_bytes()),
        ("main.rhai", script.as_bytes()),
    ]);
    let (status, _) = post_zip(app.clone(), zip).await;
    assert_eq!(status, StatusCode::OK);
    seed_oauth(&state.db, "ya29.fake");

    // Flip enabled_modules to just ["gmail"] via the vault directly
    // â€” same byte-for-byte path the SPA's PUT handler writes through.
    let now = chrono::Utc::now().timestamp();
    VaultRowStore::new(&state.db)
        .put(Some(PLUGIN_ID), "enabled_modules", br#"["gmail"]"#, now)
        .unwrap();

    // Gmail still flows. (Caller is Controller â€” every gmail tool
    // pins Controller now, so anything sub-Controller bounces at the
    // trust-floor gate before reaching the module check.)
    let _ = state
        .plugin_host
        .call_tool(
            "gmail.list_labels",
            serde_json::json!({}),
            &["*"],
            Some("Controller"),
        )
        .await
        .expect("gmail module still enabled");

    // Each of the other four modules must throw with the clear
    // "<module> module is disabled" error. Pass Controller trust so
    // we get past the trust-floor gate and exercise the module gate.
    for (tool, expected_module) in [
        ("calendar.list_calendars", "calendar"),
        ("contacts.list", "contacts"),
        ("tasks.list_lists", "tasks"),
        ("drive.search", "drive"),
    ] {
        let err = state
            .plugin_host
            .call_tool(
                tool,
                serde_json::json!({"query": "x"}),
                &["*"],
                Some("Controller"),
            )
            .await
            .unwrap_err();
        assert!(
            err.contains(&format!("{expected_module} module is disabled")),
            "expected '{expected_module} module is disabled' in error for {tool}; got: {err}",
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_google_apps_tool_rejects_sub_controller_callers() {
    // v0.3.0 trust contract: NO carve-outs. Every google-apps tool
    // touches the operator's personal Google account in some way â€”
    // mailbox, files, calendar (even freeBusy leaks the operator's
    // daily schedule, which is itself a surveillance primitive),
    // contacts, tasks. Outside principals (KnownTrusted at best
    // when they're a saved contact reaching the agent over a
    // bridged transport) must NOT be able to read or mutate any of
    // it, full stop.
    //
    // This test was previously named
    // `check_availability_is_the_only_tool_callable_from_outside_principals`
    // and pinned the v0.2.0 freeBusy carve-out. The carve-out was
    // removed in v0.3.0 after observing that scheduled-time
    // intervals are an attack primitive (you can map travel
    // windows, daily routines, etc.). Now we pin the inverse:
    // every tool, including check_availability, must reject
    // sub-Controller callers at the trust-floor gate.
    let (mock_url, _calls) = spawn_mock_api();
    let stage_dir = tempfile::tempdir().unwrap();
    let (app, state) = build_app(stage_dir.path().to_path_buf());

    let (manifest, script) = load_plugin_files(&mock_url);
    let zip = build_zip(&[
        ("plugin.toml", manifest.as_bytes()),
        ("main.rhai", script.as_bytes()),
    ]);
    let (status, _) = post_zip(app.clone(), zip).await;
    assert_eq!(status, StatusCode::OK);
    seed_oauth(&state.db, "ya29.fake");

    // Every google-apps tool â€” including the previously-carved-out
    // check_availability â€” must be rejected for a KnownTrusted caller.
    for tool in [
        "gmail.list_labels",
        "calendar.list_calendars",
        "calendar.list_events",
        "calendar.check_availability",
        "contacts.list",
        "tasks.list_lists",
        "drive.search",
    ] {
        let err = state
            .plugin_host
            .call_tool(
                tool,
                serde_json::json!({
                    "query": "x",
                    "time_min": "2026-05-11T00:00:00Z",
                    "time_max": "2026-05-12T00:00:00Z",
                }),
                &["*"],
                Some("KnownTrusted"),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_lowercase().contains("trust") || err.contains("Controller"),
            "{tool}: expected trust-floor rejection for KnownTrusted caller; got: {err}",
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn controller_floor_tool_rejects_sub_controller_caller() {
    // Adversarial: a Contact-tier principal calling gmail.send_message
    // must be rejected BEFORE the script runs. This is the security
    // invariant â€” letting a Signal contact spam mail through the
    // operator's Gmail is exactly the attack `trust_floor` exists to
    // prevent.
    let (mock_url, _calls) = spawn_mock_api();
    let stage_dir = tempfile::tempdir().unwrap();
    let (app, state) = build_app(stage_dir.path().to_path_buf());

    let (manifest, script) = load_plugin_files(&mock_url);
    let zip = build_zip(&[
        ("plugin.toml", manifest.as_bytes()),
        ("main.rhai", script.as_bytes()),
    ]);
    let (status, _) = post_zip(app.clone(), zip).await;
    assert_eq!(status, StatusCode::OK);
    seed_oauth(&state.db, "ya29.fake");

    let err = state
        .plugin_host
        .call_tool(
            "gmail.send_message",
            serde_json::json!({
                "to": "victim@example.com",
                "subject": "spam",
                "body": "x",
            }),
            &["*"],
            Some("KnownTrusted"),
        )
        .await
        .unwrap_err();
    // The trust-floor gate fires with a message naming the floor â€”
    // the exact prose lives in plugin-host; assert the salient bits.
    assert!(
        err.to_lowercase().contains("trust") || err.contains("Controller"),
        "expected trust-floor rejection for sub-Controller caller; got: {err}",
    );
}
