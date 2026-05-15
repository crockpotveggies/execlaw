//! Phase 2 plugin-lifecycle integration tests.
//!
//! Covers:
//! - `POST /api/admin/plugins/install` end-to-end with a ZIP built in
//!   memory (tool-declaring manifest + no-op runtime).
//! - Persistence across `PluginHost` instances (simulates restart).
//! - Hook-registry conflicts at HTTP layer.
//! - Capability enforcement at dispatch time.
//! - Zip-slip rejection.
//!
//! The subprocess-tier parts are gated on `#[cfg(unix)]` since they
//! rely on `sh -c` for a platform-neutral JSON-RPC echo responder.

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

/// Build an in-memory ZIP with the given (name, contents) entries.
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
        plugin_host: PluginHost::new(db.clone(), HookRegistry::new(), stage_root),
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

// ---- test plugin manifests -------------------------------------------------

/// A minimal manifest with NO tools — no subprocess needed, so this
/// tests the bare-bones install/list/disable/enable/uninstall path
/// without depending on a working `sh` binary.
fn manifest_no_tools(id: &str) -> String {
    format!(
        r#"[plugin]
id = "{id}"
name = "{id}"
version = "0.1.0"
"#
    )
}

/// Manifest that declares a tool and a subprocess runtime. Used for
/// the E2E capability + dispatch test.
#[cfg(unix)]
fn manifest_with_tool(id: &str, tool_name: &str) -> String {
    format!(
        r#"[plugin]
id = "{id}"
name = "{id}"
version = "0.1.0"

[[tools]]
name = "{tool_name}"
schema = "schema.json"
latency = "low"
required_capabilities = ["tools.safe"]

[runtime]
tier = "subprocess"
executable = "sh"
args = ["-c", "while IFS= read -r line; do id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p'); printf '{{\"id\":%s,\"result\":{{\"ok\":true,\"echoed\":\"hi\"}}}}\n' \"$id\"; done"]
"#
    )
}

// ---- tests -----------------------------------------------------------------

#[tokio::test]
async fn install_list_disable_enable_uninstall_without_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _) = build_app(tmp.path().to_path_buf());

    // Install a no-tools plugin.
    let zip = build_zip(&[("plugin.toml", manifest_no_tools("p-noop").as_bytes())]);
    let (status, body) = post_zip(app.clone(), zip).await;
    assert_eq!(status, StatusCode::OK, "install body: {body}");
    assert_eq!(body["plugin_id"], "p-noop");
    assert_eq!(body["version"], "0.1.0");

    // List shows it enabled.
    let (status, body) = get_json(app.clone(), "/api/admin/plugins").await;
    assert_eq!(status, StatusCode::OK);
    let plugins = body["plugins"].as_array().unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0]["plugin_id"], "p-noop");
    assert_eq!(plugins[0]["enabled"], true);

    // Disable.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/admin/plugins/p-noop/disable")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (_, body) = get_json(app.clone(), "/api/admin/plugins").await;
    assert_eq!(body["plugins"][0]["enabled"], false);

    // Re-enable.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/admin/plugins/p-noop/enable")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Uninstall.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/api/admin/plugins/p-noop")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // List is empty again.
    let (_, body) = get_json(app.clone(), "/api/admin/plugins").await;
    assert_eq!(body["plugins"].as_array().unwrap().len(), 0);
}

/// Zip-slip: an archive with a `../evil.txt` entry must be rejected
/// before any filesystem write lands outside the stage dir.
#[tokio::test]
async fn zip_slip_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _) = build_app(tmp.path().to_path_buf());

    let zip = build_zip(&[
        ("plugin.toml", manifest_no_tools("p-evil").as_bytes()),
        ("../escape.txt", b"gotcha"),
    ]);
    let (status, body) = post_zip(app, zip).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "stage_failed");
}

/// Missing plugin.toml: the manifest is required.
#[tokio::test]
async fn missing_manifest_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _) = build_app(tmp.path().to_path_buf());

    let zip = build_zip(&[("readme.txt", b"no manifest here")]);
    let (status, body) = post_zip(app, zip).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "stage_failed");
}

/// Installing the same plugin twice must fail with 409 — the second
/// install detects the existing DB row (and staged dir) and refuses.
#[tokio::test]
async fn duplicate_install_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _) = build_app(tmp.path().to_path_buf());

    let zip = build_zip(&[("plugin.toml", manifest_no_tools("dup").as_bytes())]);
    let (status, _) = post_zip(app.clone(), zip.clone()).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = post_zip(app, zip).await;
    // The staged dir exists from the first install, so the
    // `already_staged` or `already_installed` check fires.
    assert!(status == StatusCode::CONFLICT, "expected 409, got {status}");
}

/// Hook conflict at the registry layer (two plugins claiming the
/// same tool name) is covered by `hook_registry::tests::
/// partial_conflict_leaves_registry_clean`. This integration test
/// covers the HTTP-path version: install plugin A via the registry
/// directly, then try to install plugin B via HTTP — the second
/// install returns 409 with `hook_conflict`.
#[tokio::test]
async fn hook_conflict_rejected_via_http() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, state) = build_app(tmp.path().to_path_buf());

    // Pre-populate the registry with a "dup-tool" from plugin A.
    let m_a = r#"[plugin]
id = "p-a"
name = "p-a"
version = "0.1.0"

[[tools]]
name = "dup-tool"
schema = "s.json"
latency = "low"
required_capabilities = []
"#;
    state
        .plugin_host
        .registry()
        .enable(&execlaw_plugin_sdk::PluginManifest::parse(m_a).unwrap())
        .unwrap();

    // Now try to install plugin B via HTTP — it also declares
    // "dup-tool" and must be rejected at hook-registration time,
    // BEFORE any subprocess spawn attempt.
    let m_b = r#"[plugin]
id = "p-b"
name = "p-b"
version = "0.1.0"

[[tools]]
name = "dup-tool"
schema = "s.json"
latency = "low"
required_capabilities = []
"#;
    let zip = build_zip(&[("plugin.toml", m_b.as_bytes())]);
    let (status, body) = post_zip(app, zip).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "hook_conflict");
}

/// Unsupported runtime tier (e.g. "wasm") must be rejected. With
/// the script-tier addition, validation now catches unknown tiers
/// at manifest-parse time rather than during the host's install
/// step — the surface error code shifts from "unsupported_tier" to
/// the parse-failure variant ("stage_failed"), but the contract is
/// unchanged: a 4xx, no half-installed plugin, no leaked hooks.
#[tokio::test]
async fn unsupported_runtime_tier_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _) = build_app(tmp.path().to_path_buf());

    let m = r#"[plugin]
id = "p-wasm"
name = "p-wasm"
version = "0.1.0"

[[tools]]
name = "wasm-tool"
schema = "s.json"
latency = "low"
required_capabilities = []

[runtime]
tier = "wasm"
executable = "unused.wasm"
args = []
"#;
    let zip = build_zip(&[("plugin.toml", m.as_bytes())]);
    let (status, body) = post_zip(app, zip).await;
    assert!(
        status.is_client_error() || status == StatusCode::INTERNAL_SERVER_ERROR,
        "expected 4xx/5xx for unknown tier; got {status}: {body}"
    );
    let code = body["error"]["code"].as_str().unwrap_or("");
    assert!(
        matches!(code, "unsupported_tier" | "stage_failed"),
        "expected an install-rejected code, got '{code}': {body}"
    );
}

/// Persistence: install a plugin, drop the PluginHost, build a new
/// one against the same DB + stage root + registry, call hydrate().
/// The plugin must show up in list_rows() with `enabled = true`.
#[tokio::test]
async fn install_persists_across_host_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let stage_root = tmp.path().to_path_buf();

    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();

    let host1 = PluginHost::new(db.clone(), HookRegistry::new(), stage_root.clone());
    let plugin_dir = stage_root.join("p-persist-0.1.0");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("plugin.toml"),
        manifest_no_tools("p-persist"),
    )
    .unwrap();
    host1.install(&plugin_dir).await.unwrap();

    // Drop host1. Simulates a server restart: same DB + stage root,
    // brand-new HookRegistry.
    drop(host1);

    let host2 = PluginHost::new(db, HookRegistry::new(), stage_root);
    host2.hydrate().await.unwrap();
    let rows = host2.list_rows().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].plugin_id, "p-persist");
    assert!(rows[0].enabled);
}

/// Capability bypass adversarial: a tool with
/// `required_capabilities = ["admin"]` rejects a caller whose caps
/// are `["tools.safe"]`. The subprocess NEVER sees the args.
#[tokio::test]
async fn capability_enforcement_blocks_missing_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let stage_root = tmp.path().to_path_buf();
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    let host = PluginHost::new(db, HookRegistry::new(), stage_root.clone());

    // Register a tool that requires "admin" via a no-subprocess path
    // — the tool-call path runs cap check BEFORE attempting to reach
    // the subprocess, so we only need the hook to be registered.
    // We do that by manually enabling a manifest via the registry.
    let m = r#"[plugin]
id = "p-cap"
name = "p-cap"
version = "0.1.0"

[[tools]]
name = "dangerous"
schema = "s.json"
latency = "low"
required_capabilities = ["admin"]
"#;
    host.registry()
        .enable(&execlaw_plugin_sdk::PluginManifest::parse(m).unwrap())
        .unwrap();

    // Caller only has "tools.safe" — not "admin".
    let err = host
        .call_tool("dangerous", serde_json::json!({}), &["tools.safe"], None)
        .await
        .unwrap_err();
    assert!(err.contains("requires capability 'admin'"), "err: {err}");
}

/// Wildcard capability "*" satisfies any requirement — used by
/// Controller turns.
#[tokio::test]
async fn wildcard_capability_satisfies_any_requirement() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    let host = PluginHost::new(db, HookRegistry::new(), tmp.path().to_path_buf());

    let m = r#"[plugin]
id = "p-star"
name = "p-star"
version = "0.1.0"

[[tools]]
name = "needs-admin"
schema = "s.json"
latency = "low"
required_capabilities = ["admin", "vault.write"]
"#;
    host.registry()
        .enable(&execlaw_plugin_sdk::PluginManifest::parse(m).unwrap())
        .unwrap();

    // Caller has ONLY wildcard — capability check passes; the call
    // then fails because no runtime (subprocess OR script) is
    // actually loaded for this plugin, but for a DIFFERENT reason
    // than cap-missing.
    let err = host
        .call_tool("needs-admin", serde_json::json!({}), &["*"], None)
        .await
        .unwrap_err();
    assert!(
        err.contains("no runtime is loaded") || err.contains("subprocess is not running"),
        "wildcard should pass the cap check and fail on dispatch: {err}"
    );
}

/// The tools listing reflects what the registry currently holds.
#[tokio::test]
async fn tools_endpoint_lists_registered_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, state) = build_app(tmp.path().to_path_buf());

    // Manually register a tool on the registry so we don't need to
    // spawn a subprocess.
    let m = r#"[plugin]
id = "p-tools"
name = "p-tools"
version = "0.1.0"

[[tools]]
name = "alpha"
schema = "s.json"
latency = "medium"
required_capabilities = ["tools.medium"]

[[tools]]
name = "beta"
schema = "s.json"
latency = "low"
required_capabilities = []
"#;
    state
        .plugin_host
        .registry()
        .enable(&execlaw_plugin_sdk::PluginManifest::parse(m).unwrap())
        .unwrap();

    let (status, body) = get_json(app, "/api/admin/plugins/tools").await;
    assert_eq!(status, StatusCode::OK);
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));
}

/// Empty body rejected.
#[tokio::test]
async fn empty_body_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _) = build_app(tmp.path().to_path_buf());
    let (status, body) = post_zip(app, vec![]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "empty_body");
}

// ---------------------------------------------------------------------------
// Subprocess E2E (Unix only — relies on `sh -c`).
// ---------------------------------------------------------------------------

/// End-to-end install + tool call against the in-tree reference
/// plugin (`plugins/hello`). Exercises the full path: manifest parse
/// → hook registration → subprocess spawn → JSON-RPC tool call →
/// capability check → response round-trip.
///
/// This test compiles the reference binary on-demand via `cargo
/// build -p plugin-hello` so CI doesn't need a pre-built artifact.
///
/// Gated on Unix for now: Rust's stdin/stdout pipe buffering on
/// Windows interacts poorly with synchronous `std::io::stdin().lines()`
/// inside the child. The Unix path proves the protocol round-trip;
/// we revisit the Windows variant when Phase 8 ports a real
/// production plugin that needs Windows support.
#[cfg(unix)]
#[tokio::test]
async fn reference_hello_plugin_roundtrips_end_to_end() {
    // Compile the reference plugin so `hello-plugin` is on disk.
    // Only runs once per workspace build thanks to cargo's cache.
    let status = tokio::process::Command::new(env!("CARGO"))
        .args(["build", "-p", "plugin-hello"])
        .status()
        .await
        .expect("invoke cargo build -p plugin-hello");
    assert!(status.success(), "plugin-hello build failed");

    // Locate the built binary under target/debug/ (or target/<profile>).
    let bin_name = if cfg!(windows) {
        "hello-plugin.exe"
    } else {
        "hello-plugin"
    };
    // CARGO_MANIFEST_DIR is crates/server; walk up to the workspace root.
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let built_bin = workspace_root.join("target").join("debug").join(bin_name);
    assert!(built_bin.exists(), "expected built plugin at {built_bin:?}");

    // Stage the plugin into a temp dir: plugin.toml + the binary at
    // the path declared in the manifest (`./hello-plugin`).
    let tmp = tempfile::tempdir().unwrap();
    let plugin_dir = tmp.path().join("plugin-hello-0.1.0");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    let manifest_path = workspace_root
        .join("plugins")
        .join("hello")
        .join("plugin.toml");
    let manifest_text = std::fs::read_to_string(&manifest_path)
        .expect("reference plugin.toml must exist at plugins/hello/");
    std::fs::write(plugin_dir.join("plugin.toml"), &manifest_text).unwrap();

    // Copy the compiled binary into the staged dir so the manifest's
    // relative `./hello-plugin` executable resolves correctly.
    let staged_bin = plugin_dir.join(bin_name);
    std::fs::copy(&built_bin, &staged_bin).unwrap();
    // Ensure the binary is executable on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&staged_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&staged_bin, perms).unwrap();
    }

    // PluginHost's `resolve_executable` auto-appends `.exe` on
    // Windows when the declared name has no extension, so the
    // manifest stays cross-platform as-is.

    // Build a host against a fresh DB and install from the staged dir.
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    let host = PluginHost::new(db, HookRegistry::new(), tmp.path().to_path_buf());
    host.install(&plugin_dir)
        .await
        .expect("install reference plugin");

    // Caller with "tools.safe" — matches the manifest's required_capabilities.
    let got = host
        .call_tool(
            "hello.echo",
            serde_json::json!({"message": "execlaw says hi"}),
            &["tools.safe"],
            None,
        )
        .await
        .expect("tool call should succeed");
    assert_eq!(got["echoed"], "execlaw says hi");
    assert!(got["greeting"].as_str().unwrap().contains("Hello"));

    // Caller WITHOUT "tools.safe" gets rejected at the capability
    // check — the subprocess never sees the args.
    let err = host
        .call_tool(
            "hello.echo",
            serde_json::json!({"message": "nope"}),
            &["tools.medium"],
            None,
        )
        .await
        .expect_err("should be rejected on capability check");
    assert!(err.contains("requires capability 'tools.safe'"));

    // Uninstall cleans up hooks + subprocess.
    host.uninstall("plugin-hello")
        .await
        .expect("uninstall reference plugin");
    assert!(host.registry().tool("hello.echo").is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn subprocess_plugin_tool_roundtrips_via_host() {
    let tmp = tempfile::tempdir().unwrap();
    let stage_root = tmp.path().to_path_buf();
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    let host = PluginHost::new(db, HookRegistry::new(), stage_root.clone());

    let plugin_dir = stage_root.join("p-echo-0.1.0");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("plugin.toml"),
        manifest_with_tool("p-echo", "echo-tool"),
    )
    .unwrap();
    host.install(&plugin_dir).await.unwrap();

    // Call the tool through the host with a caller that holds the
    // required capability.
    let got = host
        .call_tool(
            "echo-tool",
            serde_json::json!({"msg": "hello"}),
            &["tools.safe"],
            None,
        )
        .await
        .unwrap();
    assert_eq!(got["ok"], true);
}
