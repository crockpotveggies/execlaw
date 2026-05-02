//! End-to-end test for the real `plugins/google-contacts/main.rhai`
//! script — loads it from disk, points the http_get_cached
//! primitive at a local mock that mimics the Google People API
//! response shape, and asserts identity.resolve + contacts.list
//! behave correctly.
//!
//! This catches:
//!   * Parse / compile errors in the actual shipped script
//!   * Bad assumptions about People-API response shape
//!   * Cache-key drift if the bearer changes
//!   * Field-extraction bugs (display_name, emails, phones)
//!
//! No real Google credentials needed; the script never knows the
//! difference between our mock and the live API.

use execlaw_script::{ScriptEngine, ScriptPlugin};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// One-shot mock server that returns canned People-API JSON to
/// every GET. Returns the bound `http://127.0.0.1:PORT` URL +
/// a shared counter the test can assert against.
fn spawn_mock_people_api(body: &'static str) -> (String, Arc<Mutex<u32>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(Mutex::new(0));
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

/// Load the real plugins/google-contacts/main.rhai but rewrite
/// the People API URL string to point at our mock server.
/// Cleanest way to test without modifying the production script.
fn load_script_against_mock(mock_url: &str) -> String {
    // Path is relative to the crate's manifest dir.
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("plugins/google-contacts/main.rhai");
    let original = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
    original.replace(
        r#""https://people.googleapis.com/v1/people/me/connections""#,
        &format!(r#""{mock_url}/v1/people/me/connections""#),
    )
}

fn build_plugin(mock_url: &str) -> ScriptPlugin {
    // Test mocks are on 127.0.0.1, so we have to bypass the SSRF
    // guard for this test only — production constructs via
    // ScriptEngine::new() which keeps loopback locked out.
    let factory = ScriptEngine::with_loopback_allowed_for_tests();
    let source = load_script_against_mock(mock_url);
    ScriptPlugin::from_source("google-contacts", &source, &factory)
        .expect("real google-contacts/main.rhai must parse cleanly")
}

fn oauth_with_token(token: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert(
        "controller".into(),
        serde_json::Value::String(token.to_owned()),
    );
    m
}

// ---------------------------------------------------------------------------
// identity_resolve

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identity_resolve_returns_null_when_no_oauth_token() {
    // No mock needed — the script short-circuits before any HTTP.
    let plugin = build_plugin("http://127.0.0.1:1");
    let r = plugin
        .identity_resolve("email", "alice@example.com", serde_json::Map::new())
        .await
        .unwrap();
    assert!(r["match"].is_null(), "expected null match, got {r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identity_resolve_returns_null_for_unsupported_transport() {
    let plugin = build_plugin("http://127.0.0.1:1");
    let r = plugin
        .identity_resolve("web", "alice", oauth_with_token("ya29.tok"))
        .await
        .unwrap();
    assert!(r["match"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identity_resolve_matches_email_case_insensitive() {
    let (url, _) = spawn_mock_people_api(SAMPLE_PEOPLE_RESPONSE);
    let plugin = build_plugin(&url);
    // Inbound handle has surrounding whitespace + uppercase chars
    // — the script's normalise path lowercases and trims.
    let r = plugin
        .identity_resolve("email", "  ALICE@Example.COM  ", oauth_with_token("ya29.tok"))
        .await
        .unwrap();
    assert_eq!(r["match"]["display_name"], "Alice Smith");
    assert_eq!(r["match"]["resolved_by"], "google-contacts");
    assert_eq!(r["match"]["trust_hint"], "Contact");
    let id = r["match"]["stable_principal_id"].as_str().unwrap();
    assert!(
        id.starts_with("pri_google_"),
        "expected pri_google_ prefix, got {id}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identity_resolve_matches_phone_with_loose_formatting() {
    let (url, _) = spawn_mock_people_api(SAMPLE_PEOPLE_RESPONSE);
    let plugin = build_plugin(&url);
    // Inbound handle in different formatting — the script's
    // digits_only path strips both sides.
    let r = plugin
        .identity_resolve("phone", "15551234567", oauth_with_token("ya29.tok"))
        .await
        .unwrap();
    assert_eq!(r["match"]["display_name"], "Alice Smith");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identity_resolve_matches_secondary_email() {
    // Alice has two emails — the second one should also match.
    let (url, _) = spawn_mock_people_api(SAMPLE_PEOPLE_RESPONSE);
    let plugin = build_plugin(&url);
    let r = plugin
        .identity_resolve(
            "email",
            "alice.smith@work.example.com",
            oauth_with_token("ya29.tok"),
        )
        .await
        .unwrap();
    assert_eq!(r["match"]["display_name"], "Alice Smith");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identity_resolve_returns_null_for_unknown_email() {
    let (url, _) = spawn_mock_people_api(SAMPLE_PEOPLE_RESPONSE);
    let plugin = build_plugin(&url);
    let r = plugin
        .identity_resolve("email", "stranger@nowhere", oauth_with_token("ya29.tok"))
        .await
        .unwrap();
    assert!(r["match"].is_null(), "got {r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identity_resolve_returns_stable_principal_id_across_calls() {
    let (url, _) = spawn_mock_people_api(SAMPLE_PEOPLE_RESPONSE);
    let plugin = build_plugin(&url);
    let r1 = plugin
        .identity_resolve("email", "alice@example.com", oauth_with_token("ya29.tok"))
        .await
        .unwrap();
    let r2 = plugin
        .identity_resolve(
            "phone",
            "+1-555-123-4567",
            oauth_with_token("ya29.tok"),
        )
        .await
        .unwrap();
    // Both lookups land on Alice; principal_id derives from
    // resourceName, so it must match regardless of which
    // identifier was queried.
    assert_eq!(
        r1["match"]["stable_principal_id"],
        r2["match"]["stable_principal_id"]
    );
}

// ---------------------------------------------------------------------------
// tool_call: contacts.list

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contacts_list_returns_full_set_with_default_limit() {
    let (url, _) = spawn_mock_people_api(SAMPLE_PEOPLE_RESPONSE);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "contacts.list",
            serde_json::json!({}),
            oauth_with_token("ya29.tok"),
        )
        .await
        .unwrap();
    let contacts = r["contacts"].as_array().unwrap();
    assert_eq!(contacts.len(), 2);
    assert_eq!(r["total"], 2);
    let alice = contacts
        .iter()
        .find(|c| c["display_name"] == "Alice Smith")
        .unwrap();
    let alice_emails = alice["emails"].as_array().unwrap();
    assert_eq!(alice_emails.len(), 2);
    assert_eq!(alice["phones"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contacts_list_respects_explicit_limit() {
    let (url, _) = spawn_mock_people_api(SAMPLE_PEOPLE_RESPONSE);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "contacts.list",
            serde_json::json!({ "limit": 1 }),
            oauth_with_token("ya29.tok"),
        )
        .await
        .unwrap();
    assert_eq!(r["contacts"].as_array().unwrap().len(), 1);
    // total reports the underlying count even when truncated.
    assert_eq!(r["total"], 2);
    assert_eq!(r["limit"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contacts_list_errors_when_no_oauth_token() {
    let plugin = build_plugin("http://127.0.0.1:1");
    let err = plugin
        .tool_call(
            "contacts.list",
            serde_json::json!({}),
            serde_json::Map::new(),
        )
        .await
        .unwrap_err();
    let s = err.to_string();
    assert!(
        s.contains("not connected"),
        "expected 'not connected' guidance, got: {s}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_tool_call_throws() {
    let plugin = build_plugin("http://127.0.0.1:1");
    let err = plugin
        .tool_call(
            "contacts.delete",
            serde_json::json!({}),
            oauth_with_token("ya29.tok"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("contacts.delete"));
}

// ---------------------------------------------------------------------------
// Cache behaviour

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_resolve_with_same_token_hits_cache() {
    let (url, calls) = spawn_mock_people_api(SAMPLE_PEOPLE_RESPONSE);
    let plugin = build_plugin(&url);
    let _ = plugin
        .identity_resolve("email", "alice@example.com", oauth_with_token("ya29.tok"))
        .await
        .unwrap();
    let _ = plugin
        .identity_resolve("email", "BOB@example.com", oauth_with_token("ya29.tok"))
        .await
        .unwrap();
    // Two resolves, ONE upstream HTTP call — the second hits the
    // host's http_get_cached layer.
    assert_eq!(*calls.lock().unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_rotation_invalidates_cache() {
    let (url, calls) = spawn_mock_people_api(SAMPLE_PEOPLE_RESPONSE);
    let plugin = build_plugin(&url);
    let _ = plugin
        .identity_resolve("email", "alice@example.com", oauth_with_token("token-A"))
        .await
        .unwrap();
    let _ = plugin
        .identity_resolve("email", "alice@example.com", oauth_with_token("token-B"))
        .await
        .unwrap();
    // The cache key includes the bearer; rotating the token
    // forces a fresh upstream fetch.
    assert_eq!(*calls.lock().unwrap(), 2);
}
