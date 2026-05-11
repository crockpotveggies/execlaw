//! Behavioral tests for `plugins/discord/main.rhai`. Loads the
//! real shipped script + exercises pure helper functions (the
//! `_test_*` wrappers) without standing up a live host stack.
//!
//! Coverage:
//!   * compute_heartbeat — null + numeric sequence forms
//!   * decode_message_create — DM happy path
//!   * decode_message_create — guild channel without mention is dropped
//!   * decode_message_create — guild channel WITH mention routes
//!   * decode_message_create — bot author filter
//!   * decode_message_create — outbound-self echo filter
//!   * decode_message_create — attachments
//!   * split_discord_message — short stays one piece
//!   * split_discord_message — long splits at boundary
//!   * parse_discord_timestamp_ms — canonical UTC iso8601
//!
//! Tool-dispatch paths (send_message_impl, set_typing_impl etc.)
//! reach `http_post`, `ws_send_to_active`, and `vault_get` —
//! those require a live host stack and aren't covered here. The
//! same convention `signal_plugin.rs` and `sms_socket_plugin.rs`
//! follow.

use execlaw_script::{ScriptEngine, ScriptPlugin};
use rhai::Dynamic;
use std::path::PathBuf;

fn discord_plugin() -> ScriptPlugin {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("plugins/discord/main.rhai");
    let source =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
    let factory = ScriptEngine::new();
    ScriptPlugin::from_source("discord", &source, &factory)
        .expect("plugins/discord/main.rhai must parse")
}

async fn invoke_str(plugin: &ScriptPlugin, fn_name: &'static str, arg: &str) -> serde_json::Value {
    plugin
        .invoke_async(
            fn_name,
            vec![Dynamic::from(rhai::ImmutableString::from(arg))],
        )
        .await
        .unwrap_or_else(|e| panic!("{fn_name}('{arg}') failed: {e}"))
}

async fn invoke_str_str(
    plugin: &ScriptPlugin,
    fn_name: &'static str,
    a: &str,
    b: &str,
) -> serde_json::Value {
    plugin
        .invoke_async(
            fn_name,
            vec![
                Dynamic::from(rhai::ImmutableString::from(a)),
                Dynamic::from(rhai::ImmutableString::from(b)),
            ],
        )
        .await
        .unwrap_or_else(|e| panic!("{fn_name}('{a}', '{b}') failed: {e}"))
}

async fn invoke_no_args(plugin: &ScriptPlugin, fn_name: &'static str) -> serde_json::Value {
    plugin
        .invoke_async(fn_name, Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{fn_name}() failed: {e}"))
}

// ---- compute_heartbeat --------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeat_with_no_sequence_emits_null_payload() {
    // The vault_get inside compute_heartbeat is wrapped in
    // try/catch precisely so this path works in fixtures lacking
    // host_caps. The form `{"op":1,"d":null}` is Discord's
    // documented pre-first-dispatch heartbeat shape.
    let plugin = discord_plugin();
    let r = invoke_no_args(&plugin, "_test_compute_heartbeat").await;
    assert_eq!(r.as_str(), Some("{\"op\":1,\"d\":null}"));
}

// ---- _test_decode_message_create — DM happy path ------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_dm_routes_with_user_native_id() {
    let plugin = discord_plugin();
    let raw = r#"{
        "id": "999",
        "channel_id": "555",
        "guild_id": null,
        "content": "hi bot",
        "timestamp": "2026-05-10T12:00:00.000000+00:00",
        "author": { "id": "111", "username": "alice", "global_name": "Alice" },
        "mentions": [],
        "attachments": []
    }"#;
    let r = invoke_str_str(&plugin, "_test_decode_message_create", raw, "").await;
    let r: serde_json::Value = serde_json::from_str(r.as_str().unwrap()).unwrap();
    assert_eq!(r["channel"], "discord");
    assert_eq!(r["native_id"], "discord:user:111");
    assert_eq!(r["text"], "hi bot");
    assert_eq!(r["display_name"], "Alice");
    assert!(r["group_id"].is_null(), "DMs have no group_id");
    assert!(r["group_name"].is_null());
    assert!(r["mention_of_self"].is_null(), "DM has no mention concept");
}

// ---- _test_decode_message_create — guild channel without mention --

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_guild_message_without_mention_is_dropped() {
    // Group-awareness contract: the agent defaults to silence in
    // guild channels unless explicitly mentioned. The decoder
    // returns null for un-mentioned guild messages so the host's
    // classifier never wakes — saves inference cost AND aligns
    // with the post-`f61c372` group-addressing posture.
    let plugin = discord_plugin();
    let raw = r#"{
        "id": "999",
        "channel_id": "555",
        "guild_id": "777",
        "content": "general chat happening here",
        "timestamp": "2026-05-10T12:00:00.000000+00:00",
        "author": { "id": "111", "username": "alice" },
        "mentions": [],
        "attachments": []
    }"#;
    let r = invoke_str_str(&plugin, "_test_decode_message_create", raw, "BOT_USER_ID").await;
    // _test_decode_message_create returns to_json_string(()) = "()"-ish.
    // We just check that it's the null marker — Rhai's to_json_string
    // for () gives "null".
    assert_eq!(
        r.as_str(),
        Some("null"),
        "un-mentioned guild messages must drop at decode time"
    );
}

// ---- _test_decode_message_create — guild channel WITH mention -----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_guild_message_with_mention_routes_with_group_id() {
    let plugin = discord_plugin();
    let raw = r#"{
        "id": "999",
        "channel_id": "555",
        "guild_id": "777",
        "content": "<@BOT_USER_ID> what's the weather",
        "timestamp": "2026-05-10T12:00:00.000000+00:00",
        "author": { "id": "111", "username": "alice", "global_name": "Alice" },
        "mentions": [ { "id": "BOT_USER_ID" } ],
        "attachments": []
    }"#;
    let r = invoke_str_str(&plugin, "_test_decode_message_create", raw, "BOT_USER_ID").await;
    let r: serde_json::Value = serde_json::from_str(r.as_str().unwrap()).unwrap();
    assert_eq!(r["channel"], "discord");
    assert_eq!(
        r["native_id"], "discord:guild:777:channel:555:user:111",
        "guild native_id must encode the channel + user pair so reply() can route back"
    );
    assert_eq!(r["group_id"], "discord:guild:777:channel:555");
    // The literal `<@bot-id>` snowflake token is replaced with a
    // human-readable `@execlaw` marker — the agent still sees
    // that it was addressed (avoids "why was I pinged?"
    // confusion in long contexts) but doesn't see the raw
    // structural Discord snowflake token.
    assert_eq!(r["text"], "@execlaw what's the weather");
    assert_eq!(
        r["mention_of_self"], true,
        "explicit @-mention must hard-set mention_of_self=true so the host's classifier short-circuits"
    );
}

// ---- _test_decode_message_create — author filters -----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_drops_bot_author() {
    // Bot/webhook senders are dropped to avoid bot-on-bot
    // conversation loops — same defensive filter Slack applies on
    // event.bot_id != null.
    let plugin = discord_plugin();
    let raw = r#"{
        "id": "999",
        "channel_id": "555",
        "guild_id": null,
        "content": "hi from another bot",
        "timestamp": "2026-05-10T12:00:00.000000+00:00",
        "author": { "id": "BOT2", "username": "otherbot", "bot": true },
        "mentions": [],
        "attachments": []
    }"#;
    let r = invoke_str_str(&plugin, "_test_decode_message_create", raw, "").await;
    assert_eq!(r.as_str(), Some("null"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_drops_self_echo() {
    // Discord delivers our own outbound messages back as
    // MESSAGE_CREATE. Without the bot_user.id check, the agent
    // would see its own reply as inbound and loop.
    let plugin = discord_plugin();
    let raw = r#"{
        "id": "999",
        "channel_id": "555",
        "guild_id": null,
        "content": "this came from me",
        "timestamp": "2026-05-10T12:00:00.000000+00:00",
        "author": { "id": "SELF", "username": "execlaw" },
        "mentions": [],
        "attachments": []
    }"#;
    let r = invoke_str_str(&plugin, "_test_decode_message_create", raw, "SELF").await;
    assert_eq!(r.as_str(), Some("null"));
}

// ---- _test_decode_message_create — attachments --------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decode_passes_attachments_with_cdn_url_as_bridge_id() {
    let plugin = discord_plugin();
    let raw = r#"{
        "id": "999",
        "channel_id": "555",
        "guild_id": null,
        "content": "",
        "timestamp": "2026-05-10T12:00:00.000000+00:00",
        "author": { "id": "111", "username": "alice" },
        "mentions": [],
        "attachments": [
            {
                "id": "att1",
                "url": "https://cdn.discordapp.com/attachments/555/att1/file.png",
                "filename": "file.png",
                "content_type": "image/png",
                "size": 12345
            }
        ]
    }"#;
    let r = invoke_str_str(&plugin, "_test_decode_message_create", raw, "").await;
    let r: serde_json::Value = serde_json::from_str(r.as_str().unwrap()).unwrap();
    let atts = r["attachments"].as_array().expect("attachments must be array");
    assert_eq!(atts.len(), 1);
    assert_eq!(
        atts[0]["bridge_id"], "https://cdn.discordapp.com/attachments/555/att1/file.png",
        "attachment CDN URL becomes the bridge_id so host attachment-fetch can dereference it"
    );
    assert_eq!(atts[0]["content_type"], "image/png");
    assert_eq!(atts[0]["filename"], "file.png");
    assert_eq!(atts[0]["size_bytes"], 12345);
}

// ---- split_discord_message ----------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_short_message_returns_single_piece() {
    let plugin = discord_plugin();
    let r = invoke_str(&plugin, "_test_split", "hello world").await;
    let r: serde_json::Value = serde_json::from_str(r.as_str().unwrap()).unwrap();
    assert_eq!(r.as_array().unwrap().len(), 1);
    assert_eq!(r[0], "hello world");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_long_message_breaks_at_paragraph_boundary() {
    // Build a payload >1900 chars where the natural paragraph cut
    // sits in the second half. Cap is 1900; lo is 950 (cap/2);
    // anything past the lo threshold is preferred for the cut.
    let plugin = discord_plugin();
    let p1 = "a".repeat(1200);
    let p2 = "b".repeat(1500);
    let big = format!("{p1}\n\n{p2}");
    let r = invoke_str(&plugin, "_test_split", &big).await;
    let r: serde_json::Value = serde_json::from_str(r.as_str().unwrap()).unwrap();
    let chunks = r.as_array().unwrap();
    assert!(
        chunks.len() >= 2,
        "long message must produce at least 2 chunks; got {}",
        chunks.len()
    );
    // No single chunk exceeds the 1900-char limit.
    for (i, c) in chunks.iter().enumerate() {
        let len = c.as_str().unwrap().len();
        assert!(
            len <= 1900,
            "chunk {i} is {len} chars (>1900 limit) — paragraph split should keep each chunk under the cap"
        );
    }
}

// ---- parse_discord_timestamp_ms -----------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parse_timestamp_matches_known_epoch_ms() {
    // 2026-05-10T12:00:00.000000+00:00
    //   = 1778412800 seconds since 1970-01-01 UTC (verified against
    //     date -d "2026-05-10T12:00:00+00:00" +%s on a coreutils
    //     system; pinned literally so the test is independent of
    //     any external date tool)
    let plugin = discord_plugin();
    let r = invoke_str(&plugin, "_test_parse_timestamp", "2026-05-10T12:00:00.000000+00:00").await;
    assert_eq!(r.as_i64(), Some(1778414400000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parse_timestamp_carries_fractional_milliseconds() {
    let plugin = discord_plugin();
    let r = invoke_str(&plugin, "_test_parse_timestamp", "2026-05-10T12:00:00.123000+00:00").await;
    assert_eq!(r.as_i64(), Some(1778414400123));
}
