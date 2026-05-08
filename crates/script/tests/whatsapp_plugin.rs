//! Behavioral tests for `plugins/whatsapp/main.rhai`. Loads the
//! real shipped script + exercises its pure helper functions
//! (`decode_event_map`, `strip_jid`) — same shape the
//! `signal_plugin.rs` test pattern uses.
//!
//! What this catches:
//!   * Parse / compile errors in the actual shipped script. The
//!     2026-05-08 group-name lookup added `resolve_group_name_cached`
//!     and webhook-handler enrichment that nothing else exercises;
//!     a broken edit there would otherwise reach production
//!     unreviewed.
//!   * Wuzapi event-shape decode regressions — the JID-stripping +
//!     IsGroup detection are easy to break with a refactor.
//!
//! What it does NOT cover:
//!   * `resolve_group_name_cached` with an actual cache hit — that
//!     calls `vault_get` which requires a host-caps wiring. Same
//!     no-host-caps limitation as the signal_plugin.rs test.
//!   * The `/group/list` HTTP round-trip on cache miss — same
//!     constraint.
//!   * `tool_call` dispatch — needs live sidecar.
//!
//! Pure-function coverage is enough to catch the parse-time +
//! decode-shape regressions that have historically mattered.

use execlaw_script::{ScriptEngine, ScriptPlugin};
use rhai::Dynamic;
use std::path::PathBuf;

fn whatsapp_plugin() -> ScriptPlugin {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("plugins/whatsapp/main.rhai");
    let source =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
    let factory = ScriptEngine::new();
    ScriptPlugin::from_source("whatsapp", &source, &factory)
        .expect("plugins/whatsapp/main.rhai must parse")
}

async fn invoke_map(
    plugin: &ScriptPlugin,
    fn_name: &'static str,
    arg: serde_json::Value,
) -> serde_json::Value {
    // Convert serde_json::Value → Rhai Dynamic so decode_event_map
    // sees a real Rhai Map (matches the production webhook handler
    // path which gets the body pre-decoded by the host).
    let dyn_arg = serde_to_dynamic(arg);
    plugin
        .invoke_async(fn_name, vec![dyn_arg])
        .await
        .unwrap_or_else(|e| panic!("{fn_name} failed: {e}"))
}

/// Convert a `serde_json::Value` into a `rhai::Dynamic`. Mirrors
/// the conversion the host's webhook dispatcher applies before
/// invoking the plugin's `on_webhook_event` handler — keeps the
/// test honest by exercising the same Map shape Rhai sees in
/// production.
fn serde_to_dynamic(v: serde_json::Value) -> Dynamic {
    use serde_json::Value;
    match v {
        Value::Null => Dynamic::UNIT,
        Value::Bool(b) => Dynamic::from(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Dynamic::from(i)
            } else if let Some(f) = n.as_f64() {
                Dynamic::from(f)
            } else {
                Dynamic::from(n.to_string())
            }
        }
        Value::String(s) => Dynamic::from(s),
        Value::Array(arr) => {
            let mut out: rhai::Array = Vec::with_capacity(arr.len());
            for item in arr {
                out.push(serde_to_dynamic(item));
            }
            Dynamic::from(out)
        }
        Value::Object(obj) => {
            let mut m = rhai::Map::new();
            for (k, v) in obj {
                m.insert(k.into(), serde_to_dynamic(v));
            }
            Dynamic::from(m)
        }
    }
}

// ---- parse smoke -----------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whatsapp_main_rhai_parses() {
    // The act of constructing the plugin compiles the source. Any
    // Rhai parse error (mismatched braces, unknown operator, etc.)
    // would surface here. This is the bare-minimum guard against a
    // commit that breaks the shipped plugin.
    let _ = whatsapp_plugin();
}

// ---- decode_event_map ------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_canonical_dm_text_event() {
    let plugin = whatsapp_plugin();
    let event = serde_json::json!({
        "type": "Message",
        "event": {
            "Info": {
                "Chat": "15551112222@s.whatsapp.net",
                "Sender": "15553334444@s.whatsapp.net",
                "PushName": "Alice",
                "IsGroup": false,
                "Timestamp": 1700000000_i64,
            },
            "Message": {
                "conversation": "hi from a whatsapp dm",
            }
        }
    });
    let out = invoke_map(&plugin, "decode_event_map", event).await;
    assert_eq!(out["channel"], "whatsapp");
    assert_eq!(out["native_id"], "15553334444");
    assert_eq!(out["display_name"], "Alice");
    assert!(out["group_id"].is_null());
    assert_eq!(out["text"], "hi from a whatsapp dm");
    // Timestamp is converted from seconds to ms.
    assert_eq!(out["timestamp_ms"], 1_700_000_000_000_i64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_group_event_sets_group_id_from_chat_jid() {
    // Group inbounds have `IsGroup: true` AND a `Chat` JID with the
    // `@g.us` suffix. The decoder picks `group_id` from the chat
    // JID and `native_id` from the (sender) JID. `group_name` is
    // left null at decode time — the webhook handler enriches it
    // via `resolve_group_name_cached` before routing.
    let plugin = whatsapp_plugin();
    let event = serde_json::json!({
        "type": "Message",
        "event": {
            "Info": {
                "Chat": "12345678901-1700000000@g.us",
                "Sender": "15553334444@s.whatsapp.net",
                "PushName": "Bob",
                "IsGroup": true,
                "Timestamp": 1700000000_i64,
            },
            "Message": {
                "conversation": "hi everyone",
            }
        }
    });
    let out = invoke_map(&plugin, "decode_event_map", event).await;
    assert_eq!(out["channel"], "whatsapp");
    assert_eq!(out["native_id"], "15553334444");
    assert_eq!(out["group_id"], "12345678901-1700000000@g.us");
    // Decoder doesn't supply the human group title — the cache-
    // backed lookup in `on_webhook_event` does. Pin so a future
    // refactor that tries to fold the lookup into the decoder
    // doesn't regress (tying it to an HTTP call would make every
    // decode depend on a live sidecar).
    assert!(out["group_name"].is_null());
    assert_eq!(out["text"], "hi everyone");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_drops_non_message_envelope() {
    // Wuzapi posts many event types — read receipts, presence,
    // history sync chunks, etc. Only `type: "Message"` is content.
    // The decoder returns Unit (Rhai null) for everything else;
    // the webhook handler 200-acks without invoking
    // `host_route_inbound`.
    let plugin = whatsapp_plugin();
    let event = serde_json::json!({
        "type": "ReadReceipt",
        "event": {
            "Info": { "Chat": "x", "Sender": "y" },
            "Message": null
        }
    });
    let out = invoke_map(&plugin, "decode_event_map", event).await;
    assert!(
        out.is_null(),
        "non-Message envelopes must decode to Unit; got: {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_drops_empty_text_with_no_attachments() {
    // Empty conversation + no attachments → Unit. Pin so the
    // text-extraction fallthrough doesn't accidentally publish a
    // zero-content turn.
    let plugin = whatsapp_plugin();
    let event = serde_json::json!({
        "type": "Message",
        "event": {
            "Info": {
                "Chat": "15551112222@s.whatsapp.net",
                "Sender": "15553334444@s.whatsapp.net",
                "PushName": "Alice",
                "IsGroup": false,
            },
            "Message": {
                "conversation": "   ",
            }
        }
    });
    let out = invoke_map(&plugin, "decode_event_map", event).await;
    assert!(
        out.is_null(),
        "empty/whitespace-only message with no attachments must decode to Unit; got: {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_picks_up_image_caption_when_conversation_empty() {
    // Image-with-caption messages put the text on
    // `Message.imageMessage.caption`, not `Message.conversation`.
    // The decoder probes both so a photo-with-caption reaches the
    // agent with the caption as its text.
    let plugin = whatsapp_plugin();
    let event = serde_json::json!({
        "type": "Message",
        "event": {
            "Info": {
                "Chat": "15551112222@s.whatsapp.net",
                "Sender": "15553334444@s.whatsapp.net",
                "PushName": "Alice",
                "IsGroup": false,
            },
            "Message": {
                "imageMessage": {
                    "url": "https://mmg.whatsapp.net/d/f/abc.enc",
                    "mimetype": "image/jpeg",
                    "fileName": "photo.jpg",
                    "fileLength": 102_400_i64,
                    "caption": "look at this",
                }
            }
        }
    });
    let out = invoke_map(&plugin, "decode_event_map", event).await;
    assert_eq!(out["text"], "look at this");
    let attachments = out["attachments"].as_array().expect("attachments array");
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["filename"], "photo.jpg");
    assert_eq!(attachments[0]["content_type"], "image/jpeg");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strip_jid_strips_at_suffix() {
    // `strip_jid` is the helper that turns
    // `15553334444@s.whatsapp.net` into `15553334444`. Used for
    // both `native_id` derivation and any tool path that needs a
    // bare phone number.
    let plugin = whatsapp_plugin();
    let v = plugin
        .invoke_async(
            "strip_jid",
            vec![Dynamic::from(rhai::ImmutableString::from(
                "15553334444@s.whatsapp.net",
            ))],
        )
        .await
        .expect("strip_jid call");
    assert_eq!(v.as_str(), Some("15553334444"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strip_jid_passes_through_unsuffixed_input() {
    let plugin = whatsapp_plugin();
    let v = plugin
        .invoke_async(
            "strip_jid",
            vec![Dynamic::from(rhai::ImmutableString::from("15553334444"))],
        )
        .await
        .expect("strip_jid call");
    assert_eq!(v.as_str(), Some("15553334444"));
}
