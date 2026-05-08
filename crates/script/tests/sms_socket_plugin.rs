//! Behavioral tests for `plugins/sms-socket/main.rhai`. Loads the
//! real shipped script + exercises pure helper functions
//! (`decode_event` via the `_test_decode` wrapper, `validate_e164`,
//! `strip_sms_prefix`, `strip_data_url_prefix`, `mask`).
//!
//! Mirrors the shape of `signal_plugin.rs`. Tool-dispatch paths
//! (send_message_impl, send_with_attachments_impl, etc.) reach into
//! `ws_send_to_active` / `host_get_attachment_bytes` host bindings;
//! those require a live host stack to exercise meaningfully and
//! are intentionally NOT covered here. The pure-helper coverage
//! catches regressions in:
//!
//!   * E.164 validation (off-by-one digit counts, missing `+`)
//!   * sms: prefix stripping
//!   * MMS attachment decoding (id / mime / filename normalization)
//!   * Empty-message-no-attachments drop path
//!   * Address-missing drop path
//!   * data URL → raw base64 conversion (used in MMS payloads)

use execlaw_script::{ScriptEngine, ScriptPlugin};
use rhai::Dynamic;
use std::path::PathBuf;

fn sms_socket_plugin() -> ScriptPlugin {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("plugins/sms-socket/main.rhai");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
    let factory = ScriptEngine::new();
    ScriptPlugin::from_source("sms-socket", &source, &factory)
        .expect("plugins/sms-socket/main.rhai must parse")
}

async fn invoke_one_str(plugin: &ScriptPlugin, fn_name: &'static str, arg: &str) -> serde_json::Value {
    plugin
        .invoke_async(
            fn_name,
            vec![Dynamic::from(rhai::ImmutableString::from(arg))],
        )
        .await
        .unwrap_or_else(|e| panic!("{fn_name}('{arg}') failed: {e}"))
}

async fn invoke_one_str_expect_throw(
    plugin: &ScriptPlugin,
    fn_name: &'static str,
    arg: &str,
    needle: &str,
) {
    let r = plugin
        .invoke_async(
            fn_name,
            vec![Dynamic::from(rhai::ImmutableString::from(arg))],
        )
        .await;
    match r {
        Ok(v) => panic!(
            "{fn_name}('{arg}') was supposed to throw containing '{needle}'; got Ok({v})"
        ),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains(needle),
                "{fn_name}('{arg}') threw '{msg}' but expected substring '{needle}'"
            );
        }
    }
}

// ---- validate_e164 -------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validate_e164_accepts_canonical_numbers() {
    let plugin = sms_socket_plugin();
    // 11-digit NANP, 12-digit UK, 8-digit (E.164 minimum is 7 digits
    // after the +) — all valid.
    for ok in &["+14165550100", "+447911123456", "+12345678"] {
        invoke_one_str(&plugin, "validate_e164", ok).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validate_e164_rejects_missing_plus() {
    let plugin = sms_socket_plugin();
    invoke_one_str_expect_throw(
        &plugin,
        "validate_e164",
        "14165550100",
        "must be E.164",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validate_e164_rejects_too_short_and_too_long() {
    let plugin = sms_socket_plugin();
    // 6 digits — below the E.164 minimum of 7.
    invoke_one_str_expect_throw(&plugin, "validate_e164", "+123456", "wrong digit count").await;
    // 16 digits — above the E.164 maximum of 15.
    invoke_one_str_expect_throw(
        &plugin,
        "validate_e164",
        "+1234567890123456",
        "wrong digit count",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validate_e164_rejects_empty() {
    let plugin = sms_socket_plugin();
    invoke_one_str_expect_throw(&plugin, "validate_e164", "", "is empty").await;
}

// ---- strip_sms_prefix ---------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strip_sms_prefix_drops_leading_sms_colon() {
    let plugin = sms_socket_plugin();
    let r = invoke_one_str(&plugin, "strip_sms_prefix", "sms:+14165550100").await;
    assert_eq!(r, "+14165550100");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strip_sms_prefix_passes_through_bare_number() {
    let plugin = sms_socket_plugin();
    let r = invoke_one_str(&plugin, "strip_sms_prefix", "+14165550100").await;
    assert_eq!(r, "+14165550100");
}

// ---- strip_data_url_prefix ----------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strip_data_url_prefix_extracts_base64_payload() {
    let plugin = sms_socket_plugin();
    // host_get_attachment_bytes returns a data URL; the gateway
    // wants only the base64 payload after the comma.
    let r = invoke_one_str(
        &plugin,
        "strip_data_url_prefix",
        "data:image/png;base64,iVBORw0KGgo=",
    )
    .await;
    assert_eq!(r, "iVBORw0KGgo=");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strip_data_url_prefix_leaves_raw_payload_alone() {
    let plugin = sms_socket_plugin();
    // No comma → no prefix to strip; pass through verbatim.
    let r = invoke_one_str(&plugin, "strip_data_url_prefix", "iVBORw0KGgo=").await;
    assert_eq!(r, "iVBORw0KGgo=");
}

// ---- decode_event (via _test_decode) ------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_canonical_sms_received() {
    let plugin = sms_socket_plugin();
    let raw = r#"{
        "type": "sms.received",
        "payload": {
            "address": "+14165550100",
            "body": "hello there",
            "receivedAt": 1700000000000,
            "messageId": "abc123"
        }
    }"#;
    let r = invoke_one_str(&plugin, "_test_decode", raw).await;
    assert_eq!(r["channel"], "sms");
    assert_eq!(r["native_id"], "+14165550100");
    assert_eq!(r["text"], "hello there");
    assert_eq!(r["timestamp_ms"], 1700000000000_i64);
    assert!(r["group_id"].is_null(), "SMS has no group concept");
    assert_eq!(
        r["display_name"], "+14165550100",
        "decoder must fall back to the phone number so the thread sidebar shows a meaningful label \
         (was previously null → SPA rendered 'New chat · abc123' for every SMS conversation)"
    );
    assert!(
        r["attachments"].as_array().unwrap().is_empty(),
        "SMS without MMS payload should have no attachments; got {:?}",
        r["attachments"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_uses_gateway_display_name_when_present() {
    // Rehydrated events carry the gateway's contact-book lookup
    // result in payload.displayName. When non-empty, we should
    // prefer it over the phone-number fallback.
    let plugin = sms_socket_plugin();
    let raw = r#"{
        "type": "sms.received",
        "payload": {
            "address": "+14165550100",
            "displayName": "Alice Smith",
            "body": "hi",
            "receivedAt": 1700000000000
        }
    }"#;
    let r = invoke_one_str(&plugin, "_test_decode", raw).await;
    assert_eq!(r["display_name"], "Alice Smith");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_falls_back_to_phone_when_gateway_display_name_empty() {
    // Live events deliver displayName="" — the gateway does NOT
    // run contact-lookup on the broadcast hot path. The decoder
    // must treat empty as missing and fall back to the phone.
    let plugin = sms_socket_plugin();
    let raw = r#"{
        "type": "sms.received",
        "payload": {
            "address": "+14165550100",
            "displayName": "",
            "body": "hi",
            "receivedAt": 1700000000000
        }
    }"#;
    let r = invoke_one_str(&plugin, "_test_decode", raw).await;
    assert_eq!(r["display_name"], "+14165550100");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_drops_event_with_missing_address() {
    let plugin = sms_socket_plugin();
    // Garbage frame without `address` — must drop, not panic.
    let raw = r#"{
        "type": "sms.received",
        "payload": { "body": "no sender", "receivedAt": 1 }
    }"#;
    let r = invoke_one_str(&plugin, "_test_decode", raw).await;
    assert!(r.is_null(), "missing address must drop the frame; got {r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_drops_empty_text_no_attachments() {
    let plugin = sms_socket_plugin();
    // No body, no attachments — nothing to route.
    let raw = r#"{
        "type": "sms.received",
        "payload": { "address": "+14165550100", "body": "", "receivedAt": 1 }
    }"#;
    let r = invoke_one_str(&plugin, "_test_decode", raw).await;
    assert!(r.is_null(), "empty text + no attachments must drop; got {r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_mms_with_attachments_preserves_metadata() {
    let plugin = sms_socket_plugin();
    let raw = r#"{
        "type": "mms.received",
        "payload": {
            "address": "+14165550100",
            "body": "look at this",
            "receivedAt": 1700000000000,
            "attachments": [
                {
                    "id": "mms-att-1",
                    "fileName": "cat.png",
                    "mimeType": "image/png",
                    "sizeBytes": 12345
                }
            ]
        }
    }"#;
    let r = invoke_one_str(&plugin, "_test_decode", raw).await;
    let atts = r["attachments"].as_array().expect("attachments array");
    assert_eq!(atts.len(), 1);
    assert_eq!(atts[0]["bridge_id"], "mms-att-1");
    assert_eq!(atts[0]["content_type"], "image/png");
    assert_eq!(atts[0]["filename"], "cat.png");
    assert_eq!(atts[0]["size_bytes"], 12345);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_mms_attachment_falls_back_to_filename_when_id_missing() {
    let plugin = sms_socket_plugin();
    // Some gateway builds emit only fileName, not id. The decoder
    // should fall back to filename as the bridge id rather than
    // dropping the attachment.
    let raw = r#"{
        "type": "mms.received",
        "payload": {
            "address": "+14165550100",
            "body": "",
            "receivedAt": 1,
            "attachments": [{ "fileName": "doc.pdf", "mimeType": "application/pdf" }]
        }
    }"#;
    let r = invoke_one_str(&plugin, "_test_decode", raw).await;
    let atts = r["attachments"].as_array().expect("attachments array");
    assert_eq!(atts.len(), 1, "fallback bridge_id must keep the attachment");
    assert_eq!(atts[0]["bridge_id"], "doc.pdf");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_drops_unknown_event_type_path() {
    // The wrapper `_test_decode` only forwards known shapes; for
    // unknown types it still calls decode_event but the decoder
    // returns Unit when the payload is missing required fields.
    // Test: a `gateway.state` envelope (no `address`, no `body`)
    // must come back null rather than panic.
    let plugin = sms_socket_plugin();
    let raw = r#"{
        "type": "gateway.state",
        "payload": { "running": true, "enabled": true, "addresses": [], "connectionCount": 1 }
    }"#;
    let r = invoke_one_str(&plugin, "_test_decode", raw).await;
    assert!(
        r.is_null(),
        "decoder should return Unit for non-message envelopes; got {r}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_handles_garbage_json_gracefully() {
    let plugin = sms_socket_plugin();
    // The wrapper catches parse errors. Production on_frame logs +
    // drops; the helper just returns Unit.
    let r = invoke_one_str(&plugin, "_test_decode", "not json {{{").await;
    assert!(r.is_null());
}

// ---- mask ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mask_hides_all_but_trailing_4() {
    let plugin = sms_socket_plugin();
    let r = invoke_one_str(&plugin, "mask", "abcdef1234567890").await;
    // 16 chars total → 12 stars + last 4.
    assert_eq!(r, "************7890");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mask_short_input_fully_obscured() {
    let plugin = sms_socket_plugin();
    let r = invoke_one_str(&plugin, "mask", "abcd").await;
    assert_eq!(r, "****");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mask_empty_passes_through() {
    let plugin = sms_socket_plugin();
    let r = invoke_one_str(&plugin, "mask", "").await;
    assert_eq!(r, "");
}
