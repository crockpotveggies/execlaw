//! End-to-end smoke test for the script-tier (Rhai) plugin runtime.
//!
//! Builds a tiny `.rhai` plugin in memory, packs it into an install
//! ZIP, posts to `/api/admin/plugins/install`, then drives an
//! `identity.resolve` through the host's dispatch path. The script
//! returns a canned match for one (transport, handle) pair and
//! null for everything else â€” proves the registry registered the
//! identity_provider hook + the dispatcher routed the call to the
//! script + the script's return JSON round-tripped cleanly.
//!
//! Cross-platform: no shell scripts, no native binaries â€” the
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
license = "Apache-2.0"

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

    // Non-match case: unknown email â†’ no match.
    let none = state
        .plugin_host
        .resolve_identity("email", "stranger@example.com")
        .await;
    assert!(none.is_empty(), "expected no matches, got {none:?}");

    // Wrong transport: phone is not in `resolves` â†’ still no match.
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
    // important thing is it does NOT return 200 â€” a half-installed
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

// ---------------------------------------------------------------------------
// Upgrade flow: ?if_existing=upgrade preserves OAuth rows; default 409s.

const MANIFEST_V1: &str = r#"
[plugin]
id = "upgrade-test"
name = "Upgrade Test"
version = "0.1.0"
description = "v1"
author = "execlaw-test"
license = "Apache-2.0"

[[oauth_accounts]]
name = "controller"
provider = "google"
scopes = ["scope-a"]

[runtime]
tier = "script"
source = "main.rhai"
"#;

const MANIFEST_V2: &str = r#"
[plugin]
id = "upgrade-test"
name = "Upgrade Test"
version = "0.2.0"
description = "v2"
author = "execlaw-test"
license = "Apache-2.0"

[[oauth_accounts]]
name = "controller"
provider = "google"
scopes = ["scope-a", "scope-b"]

[runtime]
tier = "script"
source = "main.rhai"
"#;

const TINY_SCRIPT: &str = "fn tool_call(name, args, oauth) { #{} }\n";

async fn post_zip_with_query(
    app: axum::Router,
    bytes: Vec<u8>,
    query: &str,
) -> (StatusCode, serde_json::Value) {
    let uri = if query.is_empty() {
        "/api/admin/plugins/install".to_string()
    } else {
        format!("/api/admin/plugins/install?{query}")
    };
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_a_second_time_without_if_existing_returns_409() {
    let stage_dir = tempfile::tempdir().unwrap();
    let (app, _state) = build_app(stage_dir.path().to_path_buf());

    let v1_zip = build_zip(&[
        ("plugin.toml", MANIFEST_V1.as_bytes()),
        ("main.rhai", TINY_SCRIPT.as_bytes()),
    ]);
    let (status, _) = post_zip_with_query(app.clone(), v1_zip.clone(), "").await;
    assert_eq!(status, StatusCode::OK);

    // Second install with the SAME ZIP must 409 â€” the safer
    // default protects against typo replacements.
    let (status, body) = post_zip_with_query(app, v1_zip, "").await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "default mode must reject re-install; body: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_with_if_existing_upgrade_replaces_old_version() {
    use execlaw_core::oauth::{OauthClient, OauthClientStore, OauthTokenStore, OauthTokens};

    let stage_dir = tempfile::tempdir().unwrap();
    let (app, state) = build_app(stage_dir.path().to_path_buf());

    // Install v0.1 normally.
    let v1_zip = build_zip(&[
        ("plugin.toml", MANIFEST_V1.as_bytes()),
        ("main.rhai", TINY_SCRIPT.as_bytes()),
    ]);
    let (status, body) = post_zip_with_query(app.clone(), v1_zip, "").await;
    assert_eq!(status, StatusCode::OK, "v1 install body: {body}");
    assert_eq!(body["version"], "0.1.0");

    // Operator connected the OAuth account on v0.1.
    let now = chrono::Utc::now().timestamp();
    OauthClientStore::new(&state.db)
        .upsert(&OauthClient {
            plugin_id: "upgrade-test".into(),
            account_name: "controller".into(),
            provider: "google".into(),
            client_id: "cid-survives".into(),
            client_secret: "secret-survives".into(),
            redirect_uri: "http://localhost/cb".into(),
            scopes_json: r#"["scope-a"]"#.into(),
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    OauthTokenStore::new(&state.db)
        .upsert(&OauthTokens {
            plugin_id: "upgrade-test".into(),
            account_name: "controller".into(),
            access_token: "ya29.survives".into(),
            refresh_token: Some("refresh-survives".into()),
            token_expires_at: now + 3600,
            scopes_granted: r#"["scope-a"]"#.into(),
            account_email: Some("op@example.com".into()),
            created_at: now,
            updated_at: now,
        })
        .unwrap();

    // Upgrade to v0.2 via ?if_existing=upgrade.
    let v2_zip = build_zip(&[
        ("plugin.toml", MANIFEST_V2.as_bytes()),
        ("main.rhai", TINY_SCRIPT.as_bytes()),
    ]);
    let (status, body) = post_zip_with_query(app, v2_zip, "if_existing=upgrade").await;
    assert_eq!(status, StatusCode::OK, "upgrade body: {body}");
    assert_eq!(body["version"], "0.2.0");

    // OAuth client + token rows survived.
    let client = OauthClientStore::new(&state.db)
        .get("upgrade-test", "controller")
        .unwrap()
        .expect("oauth client must survive upgrade");
    assert_eq!(client.client_id, "cid-survives");
    let token = OauthTokenStore::new(&state.db)
        .get("upgrade-test", "controller")
        .unwrap()
        .expect("oauth token must survive upgrade");
    assert_eq!(token.access_token, "ya29.survives");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_propagates_manifest_tool_descriptions_to_registry() {
    // Regression: pre-fix, every plugin tool's description was lost
    // between manifest parse and the registered tool, so chats.rs
    // synthesized "Plugin tool 'X' (latency: Y)" before shipping to
    // vLLM. Confirm the manifest description survives.
    const MANIFEST_WITH_RICH_TOOL: &str = r#"
[plugin]
id = "rich-tool-test"
name = "Rich Tool Test"
version = "0.1.0"
description = "tests that descriptions plumb through"
author = "execlaw-test"
license = "Apache-2.0"

[[tools]]
name = "rt.search"
description = "Search the operator's notes for a topic. Use when the user asks 'do I have anything about X' â€” returns up to 10 matching note ids + first-line excerpts."
latency = "low"

[runtime]
tier = "script"
source = "main.rhai"
"#;
    let stage_dir = tempfile::tempdir().unwrap();
    let (app, state) = build_app(stage_dir.path().to_path_buf());
    let zip = build_zip(&[
        ("plugin.toml", MANIFEST_WITH_RICH_TOOL.as_bytes()),
        ("main.rhai", TINY_SCRIPT.as_bytes()),
    ]);
    let (status, body) = post_zip_with_query(app, zip, "").await;
    assert_eq!(status, StatusCode::OK, "install body: {body}");

    let tool = state
        .plugin_host
        .registry()
        .tool("rt.search")
        .expect("tool registered");
    assert_eq!(
        tool.description.as_deref(),
        Some(
            "Search the operator's notes for a topic. Use when the user asks 'do I have anything about X' â€” returns up to 10 matching note ids + first-line excerpts."
        ),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_loads_per_tool_json_schema_into_the_registry() {
    // The schema file ships in the ZIP at the path the manifest's
    // `[[tools]].schema` field references; after install it must be
    // parsed and present on the RegisteredTool so chats.rs can
    // forward the real shape to vLLM.
    const MANIFEST_WITH_SCHEMA: &str = r#"
[plugin]
id = "schema-test"
name = "Schema Test"
version = "0.1.0"
description = "tests JSON schema loading"
author = "execlaw-test"
license = "Apache-2.0"

[[tools]]
name = "st.search"
description = "Search."
schema = "schemas/st_search.json"
latency = "low"

[runtime]
tier = "script"
source = "main.rhai"
"#;
    let schema_json = r#"{
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "search terms"},
            "limit": {"type": "integer", "minimum": 1, "maximum": 50}
        },
        "required": ["query"]
    }"#;
    let stage_dir = tempfile::tempdir().unwrap();
    let (app, state) = build_app(stage_dir.path().to_path_buf());
    let zip = build_zip(&[
        ("plugin.toml", MANIFEST_WITH_SCHEMA.as_bytes()),
        ("main.rhai", TINY_SCRIPT.as_bytes()),
        ("schemas/st_search.json", schema_json.as_bytes()),
    ]);
    let (status, body) = post_zip_with_query(app, zip, "").await;
    assert_eq!(status, StatusCode::OK, "install body: {body}");

    let tool = state
        .plugin_host
        .registry()
        .tool("st.search")
        .expect("tool registered");
    let schema = tool
        .schema_json
        .as_ref()
        .expect("schema_json must be loaded after install");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["query"]["type"], "string");
    assert_eq!(schema["properties"]["limit"]["maximum"], 50);
    assert_eq!(schema["required"][0], "query");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade_refreshes_config_tool_access_so_settings_tools_isnt_stale() {
    // Regression: lifecycle handlers used to skip the
    // `sync_tool_access` + `mark_plugin_tools_removed` calls, so an
    // operator who upgraded a plugin to a manifest with new tools
    // saw the OLD tool list in Settings â†’ Tools until the next
    // server restart.
    use execlaw_core::tool_access::ToolAccessStore;

    const MANIFEST_V1_TWO_TOOLS: &str = r#"
[plugin]
id = "tools-upgrade-test"
name = "Tools Upgrade Test"
version = "0.1.0"
description = "v1"
author = "execlaw-test"
license = "Apache-2.0"

[[tools]]
name = "tut.alpha"
description = "v1 tool"
latency = "low"

[[tools]]
name = "tut.beta"
description = "v1 tool kept across upgrade"
latency = "low"

[runtime]
tier = "script"
source = "main.rhai"
"#;

    const MANIFEST_V2_THREE_TOOLS: &str = r#"
[plugin]
id = "tools-upgrade-test"
name = "Tools Upgrade Test"
version = "0.2.0"
description = "v2"
author = "execlaw-test"
license = "Apache-2.0"

[[tools]]
name = "tut.beta"
description = "kept"
latency = "low"

[[tools]]
name = "tut.gamma"
description = "new in v2"
latency = "low"

[[tools]]
name = "tut.delta"
description = "also new in v2"
latency = "low"

[runtime]
tier = "script"
source = "main.rhai"
"#;

    let stage_dir = tempfile::tempdir().unwrap();
    let (app, state) = build_app(stage_dir.path().to_path_buf());

    // Install v0.1 â€” registers tut.alpha + tut.beta.
    let v1_zip = build_zip(&[
        ("plugin.toml", MANIFEST_V1_TWO_TOOLS.as_bytes()),
        ("main.rhai", TINY_SCRIPT.as_bytes()),
    ]);
    let (status, body) = post_zip_with_query(app.clone(), v1_zip, "").await;
    assert_eq!(status, StatusCode::OK, "v1 install body: {body}");

    let store = ToolAccessStore::new(&state.db);
    let alpha_v1 = store
        .get("tut.alpha")
        .unwrap()
        .expect("tut.alpha must be in config_tool_access after v1 install");
    assert!(
        alpha_v1.removed_at.is_none(),
        "fresh install must clear removed_at",
    );
    assert!(
        store.get("tut.beta").unwrap().is_some(),
        "tut.beta must appear after v1 install",
    );
    assert!(
        store.get("tut.gamma").unwrap().is_none(),
        "v2-only tool must not exist yet",
    );

    // Upgrade to v0.2. tut.alpha is dropped; tut.beta survives;
    // tut.gamma + tut.delta are new.
    let v2_zip = build_zip(&[
        ("plugin.toml", MANIFEST_V2_THREE_TOOLS.as_bytes()),
        ("main.rhai", TINY_SCRIPT.as_bytes()),
    ]);
    let (status, body) = post_zip_with_query(app, v2_zip, "if_existing=upgrade").await;
    assert_eq!(status, StatusCode::OK, "upgrade body: {body}");

    // tut.alpha must be marked removed â€” it's gone from the new
    // manifest, the dispatch gate should refuse it.
    let alpha_after = store.get("tut.alpha").unwrap().expect("row stays");
    assert!(
        alpha_after.removed_at.is_some(),
        "dropped tool must be marked removed_at after upgrade, got: {alpha_after:?}",
    );
    // tut.beta survives the upgrade with removed_at cleared.
    let beta_after = store.get("tut.beta").unwrap().expect("row exists");
    assert!(
        beta_after.removed_at.is_none(),
        "kept tool must have removed_at cleared after upgrade",
    );
    // tut.gamma + tut.delta now appear and are NOT marked removed.
    let gamma_after = store
        .get("tut.gamma")
        .unwrap()
        .expect("new v2 tool must be inserted on upgrade");
    assert!(gamma_after.removed_at.is_none());
    assert_eq!(gamma_after.source_id.as_deref(), Some("tools-upgrade-test"));
    let delta_after = store
        .get("tut.delta")
        .unwrap()
        .expect("new v2 tool must be inserted on upgrade");
    assert!(delta_after.removed_at.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn if_existing_upgrade_falls_through_to_install_when_no_existing_row() {
    // Operator's SPA flow: hit install with ?if_existing=upgrade
    // unconditionally on the second attempt after a 409. If the
    // operator had meanwhile uninstalled the plugin manually, the
    // upgrade call should NOT 404 â€” it should install fresh.
    let stage_dir = tempfile::tempdir().unwrap();
    let (app, _state) = build_app(stage_dir.path().to_path_buf());
    let v1_zip = build_zip(&[
        ("plugin.toml", MANIFEST_V1.as_bytes()),
        ("main.rhai", TINY_SCRIPT.as_bytes()),
    ]);
    let (status, body) = post_zip_with_query(app, v1_zip, "if_existing=upgrade").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "upgrade-on-empty must fall through to install; body: {body}"
    );
}
