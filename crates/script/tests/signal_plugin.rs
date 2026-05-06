//! Behavioral tests for `plugins/signal/main.rhai`. Loads the real
//! shipped script + exercises its pure helper functions
//! (`decode_frame`, `wire_recipient`) — same JSON shapes the
//! retired `crates/server/src/signal_inbound.rs` decode tests
//! covered.
//!
//! What this catches:
//!   * Parse / compile errors in the actual shipped script.
//!   * Bad assumptions about bbernhard's WS frame shape.
//!   * Group-id wire transformation bugs (the
//!     `group.<base64-of-internal-id>` form signal-cli's `/v2/send`
//!     `recipients` field requires).
//!   * Whitespace-only message handling, attachment-only inbound,
//!     and the receipt/typing-frame drop paths.
//!
//! What it does NOT cover:
//!   * tool_call dispatch into the sidecar — those calls hit
//!     `sidecar_http_*` host bindings which require a live
//!     supervised sidecar. Live integration smoke tests cover
//!     that path.
//!   * `host_route_inbound` invocation — live host required.
//!
//! The decode + wire-transform paths under test here are
//! deterministic + side-effect-free, so a unit test against the
//! shipped script catches the regressions that mattered most in
//! the retired suite (~10 of the 15 deleted decode_* tests).

use execlaw_script::{ScriptEngine, ScriptPlugin};
use rhai::Dynamic;
use std::path::PathBuf;

fn signal_plugin() -> ScriptPlugin {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("plugins/signal/main.rhai");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
    // No host_caps wired — pure decode/wire functions don't reach
    // any binding that requires them. Tool-dispatch paths would
    // throw; we don't exercise those here.
    let factory = ScriptEngine::new();
    ScriptPlugin::from_source("signal", &source, &factory)
        .expect("plugins/signal/main.rhai must parse")
}

/// Invoke a top-level Rhai function with one string arg + return
/// the result as `serde_json::Value`. Used by every test below.
async fn invoke_one(plugin: &ScriptPlugin, fn_name: &'static str, arg: &str) -> serde_json::Value {
    plugin
        .invoke_async(
            fn_name,
            vec![Dynamic::from(rhai::ImmutableString::from(arg))],
        )
        .await
        .unwrap_or_else(|e| panic!("{fn_name} failed: {e}"))
}

async fn invoke_two_str_bool(
    plugin: &ScriptPlugin,
    fn_name: &'static str,
    s: &str,
    b: bool,
) -> serde_json::Value {
    plugin
        .invoke_async(
            fn_name,
            vec![
                Dynamic::from(rhai::ImmutableString::from(s)),
                Dynamic::from(b),
            ],
        )
        .await
        .unwrap_or_else(|e| panic!("{fn_name} failed: {e}"))
}

// ---- decode_frame --------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_canonical_text_frame() {
    let plugin = signal_plugin();
    let raw = r#"{
        "envelope": {
            "source": "+15559998888",
            "sourceNumber": "+15559998888",
            "sourceName": "Alice",
            "timestamp": 1700000000000,
            "dataMessage": {
                "message": "hello",
                "groupInfo": null,
                "attachments": []
            }
        },
        "account": "+15551234567"
    }"#;
    let r = invoke_one(&plugin, "decode_frame", raw).await;
    assert_eq!(r["channel"], "signal");
    assert_eq!(r["native_id"], "+15559998888");
    assert_eq!(r["display_name"], "Alice");
    assert_eq!(r["text"], "hello");
    assert_eq!(r["timestamp_ms"], 1700000000000_i64);
    assert!(r["group_id"].is_null());
    assert!(r["group_name"].is_null());
    assert!(r["attachments"].is_array());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_drops_typing_indicator() {
    // Typing indicators arrive as a frame with no dataMessage at
    // all. Decoder must return Unit, not a half-populated map.
    let plugin = signal_plugin();
    let raw = r#"{ "envelope": { "source": "+1", "typingMessage": {"action": "STARTED"} } }"#;
    let r = invoke_one(&plugin, "decode_frame", raw).await;
    assert!(r.is_null(), "typing-only frames must drop; got {r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_drops_receipt_with_no_message_or_attachments() {
    // Read receipts / delivery acks come through with a
    // dataMessage that has no `message` and no `attachments`.
    let plugin = signal_plugin();
    let raw = r#"{
        "envelope": {
            "source": "+1",
            "dataMessage": {
                "message": null,
                "attachments": [],
                "groupInfo": null
            }
        }
    }"#;
    let r = invoke_one(&plugin, "decode_frame", raw).await;
    assert!(r.is_null(), "content-empty dataMessage must drop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_drops_whitespace_only_message() {
    // signal-cli occasionally emits a `dataMessage` whose body is
    // pure whitespace (badly-formed automation). The decoder
    // trims + drops; otherwise the agent gets a turn fired with
    // empty text.
    let plugin = signal_plugin();
    let raw = r#"{
        "envelope": {
            "source": "+1",
            "dataMessage": { "message": "   \n\t  ", "attachments": [], "groupInfo": null }
        }
    }"#;
    let r = invoke_one(&plugin, "decode_frame", raw).await;
    assert!(r.is_null(), "whitespace-only text must drop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_keeps_attachment_only_message_with_empty_text() {
    // Phase 6 regression: a contact sending a single image with
    // no caption produces a dataMessage with empty `message` and
    // a populated `attachments` array. Must surface so the
    // attachment-ingest path runs.
    let plugin = signal_plugin();
    let raw = r#"{
        "envelope": {
            "source": "+15551112222",
            "dataMessage": {
                "message": "",
                "attachments": [
                    {"id": "att-abc", "contentType": "image/jpeg", "filename": "pic.jpg", "size": 12345}
                ],
                "groupInfo": null
            }
        }
    }"#;
    let r = invoke_one(&plugin, "decode_frame", raw).await;
    assert!(!r.is_null(), "attachment-only frame must surface");
    assert_eq!(r["text"], "");
    assert_eq!(r["attachments"].as_array().unwrap().len(), 1);
    assert_eq!(r["attachments"][0]["bridge_id"], "att-abc");
    assert_eq!(r["attachments"][0]["content_type"], "image/jpeg");
    assert_eq!(r["attachments"][0]["filename"], "pic.jpg");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_strips_attachments_with_empty_id() {
    // bbernhard occasionally emits a placeholder with empty id
    // when an attachment fails server-side. Filter out so the
    // host's fetch loop doesn't hit a 404.
    let plugin = signal_plugin();
    let raw = r#"{
        "envelope": {
            "source": "+15559998888",
            "dataMessage": {
                "message": "with garbage attachment",
                "attachments": [
                    {"id": "", "contentType": "image/jpeg"},
                    {"id": "good", "contentType": "image/png"}
                ]
            }
        }
    }"#;
    let r = invoke_one(&plugin, "decode_frame", raw).await;
    let arr = r["attachments"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["bridge_id"], "good");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_picks_up_group_id_when_present() {
    let plugin = signal_plugin();
    let raw = r#"{
        "envelope": {
            "source": "+15559998888",
            "dataMessage": {
                "message": "to the group",
                "groupInfo": { "groupId": "group-base64-id" }
            }
        }
    }"#;
    let r = invoke_one(&plugin, "decode_frame", raw).await;
    assert_eq!(r["group_id"], "group-base64-id");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_picks_up_group_name_when_present_and_trims_whitespace() {
    let plugin = signal_plugin();
    let raw = r#"{
        "envelope": {
            "source": "+15559998888",
            "dataMessage": {
                "message": "hi family",
                "groupInfo": {
                    "groupId": "group-base64-id",
                    "groupName": "  Family chat  "
                }
            }
        }
    }"#;
    let r = invoke_one(&plugin, "decode_frame", raw).await;
    assert_eq!(r["group_id"], "group-base64-id");
    assert_eq!(r["group_name"], "Family chat");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_drops_empty_group_name() {
    let plugin = signal_plugin();
    let raw = r#"{
        "envelope": {
            "source": "+15559998888",
            "dataMessage": {
                "message": "hi",
                "groupInfo": { "groupId": "g", "groupName": "   " }
            }
        }
    }"#;
    let r = invoke_one(&plugin, "decode_frame", raw).await;
    assert!(r["group_name"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_returns_unit_for_unparseable_garbage() {
    let plugin = signal_plugin();
    for raw in &["", "not json", "{}", "{\"envelope\":42}"] {
        let r = invoke_one(&plugin, "decode_frame", raw).await;
        assert!(r.is_null(), "garbage must drop; got {r} for input {raw:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_falls_back_to_source_when_source_number_missing() {
    // The retired Rust path fell through to `env.source` when
    // `env.sourceNumber` was None. Pin that behavior — modern
    // signal-cli mostly omits sourceNumber, so this is the hot
    // path.
    let plugin = signal_plugin();
    let raw = r#"{
        "envelope": {
            "source": "+15551234567",
            "dataMessage": { "message": "hi", "attachments": [] }
        }
    }"#;
    let r = invoke_one(&plugin, "decode_frame", raw).await;
    assert_eq!(r["native_id"], "+15551234567");
}

// ---- wire_recipient -----------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_recipient_passes_dm_phone_through_unchanged() {
    let plugin = signal_plugin();
    let r = invoke_one(&plugin, "wire_recipient", "+15551234567").await;
    assert_eq!(r, "+15551234567");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_recipient_passes_signal_uuid_jid_through_unchanged() {
    let plugin = signal_plugin();
    let r = invoke_one(&plugin, "wire_recipient", "signal:user:abc-uuid").await;
    assert_eq!(r, "signal:user:abc-uuid");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_recipient_passes_already_prefixed_group_id_through_unchanged() {
    // If the caller already shaped the recipient as
    // `group.<base64>` (the post-transform form), don't double-
    // wrap.
    let plugin = signal_plugin();
    let r = invoke_one(&plugin, "wire_recipient", "group.SOME_BASE64=").await;
    assert_eq!(r, "group.SOME_BASE64=");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_recipient_wraps_internal_group_id_with_group_prefix_and_base64() {
    // The motivating bug fix from this whole Phase B: signal-cli's
    // outbound /v2/send rejects the inbound `internal_id` form
    // (single-base64 ending in `=`) — recipients must be
    // `group.` + base64-of-bytes-of-internal-id. The plugin's
    // wire_recipient now owns this transform.
    let plugin = signal_plugin();
    let internal_id = "KrOVgwZ5mWq17pGg1W5rmx/BW+Uun6pCT41o7EfiHbA=";
    let r = invoke_one(&plugin, "wire_recipient", internal_id).await;
    let s = r.as_str().expect("wire_recipient must return a string");
    assert!(
        s.starts_with("group."),
        "trailing-= internal_ids must be wrapped with group.<base64>; got {s}"
    );
    // Decode the base64 portion — must round-trip back to the
    // original internal_id bytes.
    use base64::Engine;
    let payload = s.trim_start_matches("group.");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .expect("group. payload must be valid base64");
    assert_eq!(decoded, internal_id.as_bytes());
}

// ---- map_attachments -----------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn map_attachments_preserves_metadata_fields() {
    // Pin via decode_frame since map_attachments takes a Rhai
    // array (not directly invokable with a JSON string from the
    // outside test surface).
    let plugin = signal_plugin();
    let raw = r#"{
        "envelope": {
            "source": "+1",
            "dataMessage": {
                "message": "msg",
                "attachments": [
                    {"id": "a1", "contentType": "image/jpeg", "filename": "p.jpg", "size": 100}
                ]
            }
        }
    }"#;
    let r = invoke_one(&plugin, "decode_frame", raw).await;
    let a = &r["attachments"][0];
    assert_eq!(a["bridge_id"], "a1");
    assert_eq!(a["content_type"], "image/jpeg");
    assert_eq!(a["filename"], "p.jpg");
    assert_eq!(a["size_bytes"], 100);
}

// Silences unused-warning for a helper that's reserved for a
// future test (signal.set_typing dispatch shape) once a
// host_caps mocking surface lands.
#[allow(dead_code)]
async fn _reserved_for_typing_test(plugin: &ScriptPlugin) {
    let _ = invoke_two_str_bool(plugin, "wire_recipient", "x", false).await;
}
