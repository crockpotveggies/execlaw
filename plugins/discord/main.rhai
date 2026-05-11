// discord — Discord transport via the official Bot Gateway WebSocket.
//
// Topology (mirrors Slack Socket Mode, NOT Signal/WhatsApp sidecar):
//   * Plugin opens a long-lived bidirectional WSS to Discord's
//     gateway (URL discovered via GET /gateway/bot). No sidecar.
//   * Outbound messages go over the REST API at discord.com/api;
//     the gateway is inbound-only-with-heartbeats.
//   * Heartbeats are application-layer (op 1 JSON), not RFC-6455
//     Ping/Pong (which the host auto-handles). They're scheduled
//     via the `ws_set_keepalive` binding the way Slack uses
//     `ws_send` — Discord docs require the interval to come from
//     the server's Hello frame, so we register the keepalive from
//     inside on_frame on each (re)connect.
//
// Identity model — single bot token spans every guild the bot is
// invited to. Multi-guild is automatic: GUILD_CREATE dispatches
// populate our local guild list as Discord pushes them after Ready.
//
// Vault keys (plugin-scoped):
//   * `bot_token` — string. "Bot <token>" is the Authorization
//                   form expected by every REST + gateway-Identify
//                   call.
//   * `bot_user`  — JSON {id, username, application_id}; cached
//                   from /users/@me after token validation. The
//                   `id` is what we match in MESSAGE_CREATE
//                   mentions[].id to populate mention_of_self.
//   * `last_sequence` — last "s" value seen on an op-0 Dispatch
//                       frame; rides every heartbeat. Cleared on
//                       fresh Identify.
//   * `session`   — JSON {session_id, resume_gateway_url} from
//                   READY. Stored for future Resume support; v0.1
//                   does NOT Resume (would need close-code
//                   surfacing from the host — see plan §3).
//   * `guilds`    — JSON map {guild_id: {name, channels[]}} —
//                   populated by GUILD_CREATE + READY.
//   * `dm_channels` — JSON map {user_id: channel_id} cached after
//                     POST /users/@me/channels lookups so we don't
//                     re-create DM channels every outbound.
//   * `last_dispatch_message` — JSON map
//                     {channel_id: message_id} for reply()'s
//                     message_reference auto-context.
//
// Reconnect policy (v0.1):
//   * Host's WS consumer auto-reconnects on any disconnect with
//     capped exponential backoff. Plugin treats every Hello as a
//     fresh session: clear `last_sequence` + `session`, send a
//     fresh Identify, re-install ws_set_keepalive with the new
//     interval. Loses Resume-optimization but is correct under
//     all close codes (4000-4014) without needing close-code
//     visibility in Rhai. v0.2: surface close codes → enable
//     Resume.

const DISCORD_API = "https://discord.com/api/v10";
const DISCORD_USER_AGENT = "DiscordBot (https://github.com/anthropics/execlaw, 0.1.0)";
const MAX_DISCORD_MESSAGE_CHARS = 1900;

// Gateway opcodes (https://discord.com/developers/docs/topics/gateway-events#payload-structure)
const OP_DISPATCH       = 0;
const OP_HEARTBEAT      = 1;
const OP_IDENTIFY       = 2;
const OP_RESUME         = 6;
const OP_RECONNECT      = 7;
const OP_INVALID_SESSION = 9;
const OP_HELLO          = 10;
const OP_HEARTBEAT_ACK  = 11;

// Identify intents bitmask.
//   GUILDS           (1 << 0)  = 1     — GUILD_CREATE etc.
//   GUILD_MESSAGES   (1 << 9)  = 512   — MESSAGE_CREATE in guild channels
//   DIRECT_MESSAGES  (1 << 12) = 4096  — MESSAGE_CREATE in DMs
//   MESSAGE_CONTENT  (1 << 15) = 32768 — message.content / .attachments / .mentions
//                                       (PRIVILEGED — operator must enable
//                                       in dev portal → Bot → Privileged Gateway
//                                       Intents)
const IDENTIFY_INTENTS = 37377;  // 1 + 512 + 4096 + 32768

// =====================================================================
// LIFECYCLE
// =====================================================================

fn on_enable() {
    let token = vault_get("bot_token");
    if token == () || token == "" {
        log_info("discord: no bot_token in vault — visit Settings → Plugins → Discord to configure");
        return;
    }
    // Discord rotates the gateway URL frequently (CDN load balancing) so we
    // ALWAYS fetch a fresh URL on enable rather than caching across restarts.
    // GET /gateway/bot also tells us if our token is still valid (401 if not)
    // and surfaces session_start_limit — useful for diagnostics.
    let gw = ();
    try {
        gw = http_get(DISCORD_API + "/gateway/bot", #{}, "", #{
            "Authorization": "Bot " + token,
            "User-Agent": DISCORD_USER_AGENT,
        });
    } catch (e) {
        log_warn("discord: GET /gateway/bot failed: " + e + " — check bot_token");
        return;
    }
    if gw == () || gw.url == () || gw.url == "" {
        log_warn("discord: /gateway/bot returned no url; response: " + gw);
        return;
    }
    let gateway_url = gw.url + "/?v=10&encoding=json";
    log_info("discord: subscribing to gateway at " + gw.url);
    // Clear per-session state on (re)enable. last_sequence is
    // session-scoped per Discord; if we don't clear, a stale
    // sequence on first heartbeat after a token change is
    // interpreted as "this is an old session" and the server
    // sends Invalid Session.
    vault_put("last_sequence", "");
    vault_put("session", "");
    let _handle = ws_subscribe_bidi(gateway_url, "on_frame");
    ()
}

// =====================================================================
// INBOUND
// =====================================================================

fn on_frame(handle, raw) {
    let env;
    try {
        env = parse_json(raw);
    } catch (e) {
        log_warn("discord: parse_json failed: " + e);
        return;
    }
    if env == () { return; }

    let op = env.op;
    if op == () { return; }

    // Cache the sequence on every Dispatch BEFORE doing anything
    // else with the frame. Heartbeats carry the latest sequence;
    // dropping it on the floor mid-handler would break the gateway
    // contract on the next tick.
    if op == 0 && env.s != () {
        vault_put("last_sequence", env.s.to_string());
    }

    if op == 10 {
        // Hello — carries heartbeat_interval (ms). Install the
        // keepalive timer + send Identify in that order so the
        // first heartbeat is jittered properly (Discord docs
        // require this to avoid the thundering-herd on mass
        // reconnect).
        let d = env.d;
        let interval = if d != () && d.heartbeat_interval != () {
            d.heartbeat_interval
        } else {
            41250  // Discord's documented default; safe fallback.
        };
        log_info("discord: Hello received; heartbeat_interval=" + interval + "ms");
        let _ok = ws_set_keepalive(handle, interval, "compute_heartbeat");
        send_identify(handle);
        return;
    }

    if op == 1 {
        // Server-requested heartbeat — send one immediately. The
        // periodic timer also keeps running; one extra tick is
        // fine (Discord doesn't penalize over-pinging).
        let payload = compute_heartbeat();
        if payload != "" {
            ws_send(handle, payload);
        }
        return;
    }

    if op == 11 {
        // Heartbeat ACK — we don't track ack-pending vs ack-received
        // in v0.1. The host's WS consumer handles dead-connection
        // detection (TCP keepalive + RFC-6455 Ping/Pong + read
        // timeout). If Discord stops ACKing AND stops sending data,
        // the socket times out and we reconnect from there.
        return;
    }

    if op == 7 {
        // Reconnect — server is asking us to gracefully reconnect.
        //
        // CRITICAL: We deliberately do NOT call `handle.close()` here.
        // The host's WS consumer-loop cancel token is permanent
        // (close() => cancel.cancel() => `while !cancel.is_cancelled()`
        // exits, the task ends, no reconnect ever happens). Instead,
        // Discord itself sends a Close frame immediately after op 7;
        // the consumer loop sees that, breaks its inner select!, and
        // the outer reconnect loop kicks in with the standard backoff.
        // Our role is just to clear session-scoped state so the next
        // Hello triggers a fresh Identify (we don't Resume in v0.1).
        log_info("discord: server-requested Reconnect (op 7); awaiting socket close");
        vault_put("last_sequence", "");
        vault_put("session", "");
        return;
    }

    if op == 9 {
        // Invalid Session — `d` is true if the session is
        // resumable (we don't Resume in v0.1, so we treat both
        // values the same: drop, wait, re-Identify on next Hello).
        //
        // Same close-handling note as op 7: Discord closes the socket
        // itself. We must NOT call handle.close() — that would cancel
        // the consumer task permanently and the bot would go silent
        // until the operator disable+re-enable cycles the plugin.
        //
        // Discord docs recommend waiting 1-5 random seconds before
        // reconnecting after op 9 to avoid hammering the gateway.
        // Our host's WS_MIN_BACKOFF is 500ms; the consumer's
        // exponential backoff will land in the right window if
        // op 9 thrashes (a single op-9 burst typically resolves on
        // the second attempt anyway).
        log_warn("discord: Invalid Session (op 9); session dropped, will re-Identify on reconnect");
        vault_put("last_sequence", "");
        vault_put("session", "");
        return;
    }

    if op != 0 { return; }  // Other ops we don't handle (none currently).

    // op == 0 Dispatch — `t` is the event name, `d` is the payload.
    let t = env.t;
    let d = env.d;
    if t == () || d == () { return; }

    if t == "READY" {
        handle_ready(d);
        return;
    }
    if t == "RESUMED" {
        log_info("discord: session resumed");
        return;
    }
    if t == "GUILD_CREATE" {
        handle_guild_create(d);
        return;
    }
    if t == "MESSAGE_CREATE" {
        handle_message_create(d);
        return;
    }
    // Other dispatches (MESSAGE_UPDATE, MESSAGE_DELETE, REACTION_*,
    // TYPING_START, PRESENCE_UPDATE, …) — drop in v0.1.
}

// Periodic heartbeat callback registered with ws_set_keepalive.
// MUST return a String — the keepalive task writes that string
// verbatim as a text frame. Empty string skips the tick (we use
// that during the brief window between socket-up and Hello-seen).
//
// vault_get wrapped in try/catch so a torn vault (or test
// fixtures running without host_caps wired) returns the safe
// null-sequence form rather than throwing — Discord accepts a
// null `d` on heartbeat before the first dispatch.
fn compute_heartbeat() {
    let s = ();
    try { s = vault_get("last_sequence"); } catch (e) { s = (); }
    if s == () || s == "" {
        return "{\"op\":1,\"d\":null}";
    }
    let parsed = host_parse_int(s);
    if parsed == () {
        return "{\"op\":1,\"d\":null}";
    }
    "{\"op\":1,\"d\":" + parsed + "}"
}

fn send_identify(handle) {
    let token = vault_get("bot_token");
    if token == () || token == "" {
        log_warn("discord: send_identify with no bot_token in vault — aborting");
        handle.close();
        return;
    }
    let identify = #{
        "op": 2,
        "d": #{
            "token": token,
            "intents": 37377,
            "properties": #{
                "os": "linux",
                "browser": "execlaw",
                "device": "execlaw",
            },
            "presence": #{
                "status": "online",
                "afk": false,
                "activities": [],
                "since": (),
            },
        },
    };
    ws_send(handle, to_json_string(identify));
    log_info("discord: Identify sent");
}

fn handle_ready(d) {
    // READY carries session_id + resume_gateway_url + user{} + guilds[].
    // We cache the session so a future v0.2 Resume can find it,
    // but don't act on it in v0.1.
    let session = #{};
    if d.session_id != () { session.session_id = d.session_id; }
    if d.resume_gateway_url != () { session.resume_gateway_url = d.resume_gateway_url; }
    vault_put("session", to_json_string(session));

    if d.user != () {
        let user = d.user;
        let me = #{};
        if user.id != () { me.id = user.id; }
        if user.username != () { me.username = user.username; }
        if user.application_id != () { me.application_id = user.application_id; }
        else if user.id != () { me.application_id = user.id; }
        vault_put("bot_user", to_json_string(me));
        let uname = if user.username != () { user.username } else { user.id };
        log_info("discord: Ready — bot=" + uname + " session=" + session.session_id);
    }

    // Initial guild list (unavailable; will be filled by
    // GUILD_CREATE dispatches that follow Ready).
    let guilds = #{};
    vault_put("guilds", to_json_string(guilds));
}

fn handle_guild_create(d) {
    // GUILD_CREATE arrives once per guild on connect. Cache name
    // + text channels so `discord.list_channels` works without
    // re-fetching.
    if d.id == () { return; }
    let raw = vault_get("guilds");
    let m = #{};
    if raw != () && raw != "" {
        try { m = parse_json(raw); } catch (e) { m = #{}; }
        if type_of(m) != "map" { m = #{}; }
    }
    let entry = #{};
    entry.name = if d.name != () { d.name } else { d.id };
    let chans = [];
    if d.channels != () && type_of(d.channels) == "array" {
        for c in d.channels {
            if c.id == () { continue; }
            // type 0 = GUILD_TEXT, 5 = GUILD_ANNOUNCEMENT — both
            // accept bot writes. Skip voice (2), categories (4),
            // and threads (10/11/12) in v0.1.
            if c.type != 0 && c.type != 5 { continue; }
            chans.push(#{
                "id": c.id,
                "name": if c.name != () { c.name } else { c.id },
            });
        }
    }
    entry.channels = chans;
    m[d.id] = entry;
    vault_put("guilds", to_json_string(m));
}

fn handle_message_create(d) {
    let bot_user = load_bot_user();
    let inbound = decode_message_create(d, bot_user);
    if inbound == () { return; }
    // Cache message id for reply()'s message_reference. Done
    // AFTER decode succeeds so we don't track ids for messages we
    // filtered out (self-echo, bot author, non-mentioned guild
    // banter) — that map is purely for reply-in-place context.
    cache_reply_target(d.channel_id, d.id);

    try {
        host_route_inbound(inbound);
    } catch (e) {
        log_warn("discord: host_route_inbound rejected: " + e);
    }
}

fn decode_message_create(d, bot_user) {
    try {
        // Self-echo filter — Discord delivers our own outbound as
        // MESSAGE_CREATE too. Without this the agent sees its own
        // reply as inbound and loops.
        if bot_user != () && bot_user.id != () && d.author != () && d.author.id == bot_user.id {
            return ();
        }
        // Bot/webhook author filter — avoids bot-on-bot
        // conversation loops with other bots in the same channel.
        if d.author != () && d.author.bot == true {
            return ();
        }
        let text = if d.content != () { d.content } else { "" };
        let trimmed = trim(text);
        let has_attachments = d.attachments != () && type_of(d.attachments) == "array" && !d.attachments.is_empty();
        if trimmed == "" && !has_attachments { return (); }

        let is_dm = d.guild_id == () || d.guild_id == "";

        // Mention detection from the structured mentions[] array
        // (population of which requires the MESSAGE_CONTENT
        // privileged intent — operator must enable in dev portal).
        let mention_of_self = ();
        if !is_dm && bot_user != () && bot_user.id != () && d.mentions != () && type_of(d.mentions) == "array" {
            for u in d.mentions {
                if u.id != () && u.id == bot_user.id {
                    mention_of_self = true;
                    break;
                }
            }
        }

        // In guild channels, the agent should default to silence
        // unless mentioned. Drop non-mention messages entirely so
        // they don't burn classifier cycles on the host.
        if !is_dm && mention_of_self != true {
            return ();
        }

        // Strip the literal "<@bot-id>" / "<@!bot-id>" mention
        // tokens out of the agent's view of the text — they're
        // structural, not content.
        let normalized = if mention_of_self == true && bot_user != () && bot_user.id != "" {
            strip_bot_mentions(text, bot_user.id)
        } else { text };

        let attachments = [];
        if has_attachments {
            for a in d.attachments {
                if a.id == () { continue; }
                let url = if a.url != () { a.url } else { a.proxy_url };
                if url == () || url == "" { continue; }
                attachments.push(#{
                    "bridge_id": url,
                    "content_type": a.content_type,
                    "filename": a.filename,
                    "size_bytes": a.size,
                });
            }
        }

        let group_id = if is_dm {
            ()
        } else {
            "discord:guild:" + d.guild_id + ":channel:" + d.channel_id
        };
        let group_name = if is_dm {
            ()
        } else {
            lookup_channel_name(d.guild_id, d.channel_id)
        };

        let display_name = ();
        if d.author != () {
            if d.member != () && d.member.nick != () && d.member.nick != "" {
                display_name = d.member.nick;
            } else if d.author.global_name != () && d.author.global_name != "" {
                display_name = d.author.global_name;
            } else if d.author.username != () && d.author.username != "" {
                display_name = d.author.username;
            }
        }

        return #{
            "channel": "discord",
            "native_id": if is_dm { "discord:user:" + d.author.id }
                         else { "discord:guild:" + d.guild_id + ":channel:" + d.channel_id + ":user:" + d.author.id },
            "display_name": display_name,
            "group_id": group_id,
            "group_name": group_name,
            "text": trim(normalized),
            "timestamp_ms": parse_discord_timestamp_ms(d.timestamp),
            "attachments": attachments,
            "mention_of_self": mention_of_self,
        };
    } catch (e) {
        log_warn("discord: undecodable MESSAGE_CREATE: " + e);
        return ();
    }
}

// =====================================================================
// TOOL DISPATCH
// =====================================================================

fn tool_call(name, args, oauth) {
    if name == "discord.send_message"          { return send_message_impl(args); }
    if name == "discord.reply"                 { return reply_impl(args); }
    if name == "discord.list_channels"         { return list_channels_impl(); }
    if name == "discord.set_typing"            { return set_typing_impl(args); }
    if name == "discord.send_with_attachments" { return send_with_attachments_impl(args); }
    if name == "discord.fetch_attachment"      { return fetch_attachment_impl(args); }
    throw "unknown tool '" + name + "'";
}

fn send_message_impl(args) {
    let to = args.to;
    let text = args.text;
    if to == () || to == "" { throw "discord.send_message requires `to`"; }
    if text == () || text == "" { throw "discord.send_message requires `text`"; }
    let token = vault_get("bot_token");
    if token == () || token == "" { throw "discord.send_message: no bot_token configured"; }
    let stripped = strip_discord_prefix(to);
    let channel_id = resolve_channel_id(stripped, token);
    if channel_id == () || channel_id == "" {
        throw "discord.send_message: could not resolve a channel for '" + to + "'";
    }
    // Auto-apply message_reference when we have a cached inbound
    // message id for this channel (reply-in-place behavior).
    let reply_to = lookup_reply_target(channel_id);
    let chunks = split_discord_message(text);
    let last = ();
    for chunk in chunks {
        let body = #{
            "content": chunk,
            // Safety guard — restrict mention parsing to user
            // pings only. Without this, an agent-authored "hey
            // @everyone please look" would actually @-everyone
            // the entire guild. parse=[users] means user mentions
            // resolve normally (@-pings still work in replies)
            // but @everyone / @here / @&role text becomes inert.
            "allowed_mentions": #{ "parse": ["users"] },
        };
        if reply_to != () && reply_to != "" {
            body.message_reference = #{
                "channel_id": channel_id,
                "message_id": reply_to,
                "fail_if_not_exists": false,
            };
            // Only attach to the first chunk so multi-part long
            // replies don't all point at the same parent.
            reply_to = ();
        }
        last = http_post(
            DISCORD_API + "/channels/" + channel_id + "/messages",
            body,
            "",
            #{
                "Authorization": "Bot " + token,
                "User-Agent": DISCORD_USER_AGENT,
            },
        );
        if last == () || last.id == () {
            throw "POST /channels/" + channel_id + "/messages failed: " + last;
        }
    }
    if last != () && last.id != () { last.id.to_string() } else { "ok" }
}

fn reply_impl(args) {
    if args.to == () || args.to == "" {
        throw "discord.reply requires `to` — host fills this from the conversation's binding";
    }
    send_message_impl(args)
}

fn list_channels_impl() {
    let raw = vault_get("guilds");
    let out = [];
    if raw != () && raw != "" {
        try {
            let m = parse_json(raw);
            if type_of(m) == "map" {
                for guild_id in m.keys() {
                    let g = m[guild_id];
                    if g == () || g.channels == () { continue; }
                    for c in g.channels {
                        out.push(#{
                            "id": c.id,
                            "name": "#" + c.name,
                            "guild": g.name,
                            "is_dm": false,
                        });
                    }
                }
            }
        } catch (e) {
            log_warn("discord: list_channels parse failed: " + e);
        }
    }
    let dm_raw = vault_get("dm_channels");
    if dm_raw != () && dm_raw != "" {
        try {
            let m = parse_json(dm_raw);
            if type_of(m) == "map" {
                for user_id in m.keys() {
                    out.push(#{
                        "id": m[user_id],
                        "name": "DM " + user_id,
                        "guild": (),
                        "is_dm": true,
                    });
                }
            }
        } catch (e) {}
    }
    #{ "channels": out, "ack_text": "Listed " + out.len() + " Discord channel(s)." }
}

fn set_typing_impl(args) {
    // Discord supports POST /channels/{id}/typing — fires a
    // typing indicator for ~10 seconds or until the bot sends a
    // message, whichever comes first. There is NO endpoint to
    // clear typing manually, so active=false is a no-op.
    if args == () { return #{ "ok": true }; }
    if args.active != true { return #{ "ok": true }; }
    let to = args.to;
    if to == () || to == "" { return #{ "ok": true }; }
    let token = vault_get("bot_token");
    if token == () || token == "" { return #{ "ok": true }; }
    let channel_id = resolve_channel_id(strip_discord_prefix(to), token);
    if channel_id == () || channel_id == "" { return #{ "ok": true }; }
    try {
        http_post(
            DISCORD_API + "/channels/" + channel_id + "/typing",
            #{},
            "",
            #{
                "Authorization": "Bot " + token,
                "User-Agent": DISCORD_USER_AGENT,
            },
        );
    } catch (e) {
        // Best-effort; typing indicator failure shouldn't surface.
        log_warn("discord: typing trigger failed: " + e);
    }
    #{ "ok": true }
}

fn send_with_attachments_impl(args) {
    let to = args.to;
    let text = if args.text != () { args.text } else { "" };
    let attachments = args.attachments;
    if to == () || to == "" { throw "discord.send_with_attachments requires `to`"; }
    if attachments == () || type_of(attachments) != "array" || attachments.is_empty() {
        return send_message_impl(#{ "to": to, "text": text });
    }
    // v0.1 fallback — no multipart binding. Mirrors slack's
    // approach: stitch a preview-line per attachment into the
    // text body so the agent's send doesn't fail outright. The
    // operator sees the attachment metadata; full file delivery
    // lands when an `http_post_multipart` binding (or Discord
    // attachments-staging flow) is added.
    log_warn("discord.send_with_attachments: v0.1 fallback — sending data-URL preview lines. Real multipart upload needs an http_post_multipart binding (follow-up).");
    let body_lines = if text != "" { text + "\n\n" } else { "" };
    let i = 0;
    for aid in attachments {
        let a = host_get_attachment_bytes(aid);
        body_lines += "Attachment " + (i + 1) + " (" + a.mime_type + ", " + a.size_bytes + " bytes): "
            + a.data_url.sub_string(0, 80) + "…\n";
        i += 1;
    }
    send_message_impl(#{ "to": to, "text": trim(body_lines) })
}

fn fetch_attachment_impl(args) {
    let attachment_id = args.attachment_id;
    if attachment_id == () || attachment_id == "" {
        throw "discord.fetch_attachment requires `attachment_id`";
    }
    // Discord attachment URLs are publicly fetchable — no auth
    // header needed. But we don't have a "GET → raw bytes" Rhai
    // binding for arbitrary external URLs (only sidecar_*).
    // v0.1: not implemented; the inbound ingest path captures
    // attachments lazily via the bridge_id-as-URL convention.
    throw "discord.fetch_attachment: not implemented in v0.1 — inbound attachments are referenced by CDN URL, which the host's attachment-fetch path can dereference directly";
}

// =====================================================================
// HELPERS
// =====================================================================

fn strip_discord_prefix(s) {
    if s == () || s == "" { return s; }
    if s.starts_with("discord:") {
        let inner = s.sub_string(8);
        // Patterns we accept:
        //   discord:user:<user_id>
        //   discord:channel:<channel_id>
        //   discord:guild:<guild_id>:channel:<channel_id>
        //   discord:guild:<g>:channel:<c>:user:<u>
        // Pull the last `<channel_id>` if present, else `<user_id>`.
        // The full-form group native_ids land here on reply().
        if inner.starts_with("user:") {
            return "user:" + inner.sub_string(5);
        }
        if inner.starts_with("channel:") {
            return inner.sub_string(8);
        }
        // guild:...:channel:CID (:user:UID)?
        let cidx = inner.index_of(":channel:");
        if cidx >= 0 {
            let after = inner.sub_string(cidx + 9);
            let uidx = after.index_of(":user:");
            if uidx >= 0 {
                // "channel:c:user:u" → we want the user-id for DM
                // routing; but only IF the original conversation
                // was a guild channel. We let resolve_channel_id
                // pick the right path — preserve everything after
                // ":channel:" as "<c>:user:<u>" and decide there.
                return after;
            }
            return after;
        }
        return inner;
    }
    s
}

// Map a `to` value the agent supplied (a channel id, a user id,
// a fully-qualified JID, or a guild-prefixed binding) to a Discord
// channel snowflake we can POST to. For DM targets, lazily creates
// (and caches) the DM channel via POST /users/@me/channels.
fn resolve_channel_id(to, token) {
    if to == () || to == "" { return (); }
    // Strip "user:" leading hint.
    if to.starts_with("user:") {
        return get_or_create_dm_channel(to.sub_string(5), token);
    }
    // "<channel_id>:user:<user_id>" — channel form wins; agent
    // is replying inside the same guild channel the inbound came
    // from.
    let uidx = to.index_of(":user:");
    if uidx > 0 {
        return to.sub_string(0, uidx);
    }
    // Bare snowflake — could be a channel or a user. Discord
    // snowflakes are indistinguishable shape-wise; we treat bare
    // ids as channel ids and let the agent disambiguate by
    // passing "discord:user:..." when it means a user. If the
    // POST fails, the operator will see the channel-not-found
    // error in logs.
    to
}

fn get_or_create_dm_channel(user_id, token) {
    if user_id == () || user_id == "" { return (); }
    let cache_raw = vault_get("dm_channels");
    if cache_raw != () && cache_raw != "" {
        try {
            let m = parse_json(cache_raw);
            if type_of(m) == "map" && m[user_id] != () && m[user_id] != "" {
                return m[user_id];
            }
        } catch (e) {}
    }
    // Create. Discord returns the channel object whose id is
    // stable for the (bot, user) pair — safe to cache forever.
    let resp = ();
    try {
        resp = http_post(
            DISCORD_API + "/users/@me/channels",
            #{ "recipient_id": user_id },
            "",
            #{
                "Authorization": "Bot " + token,
                "User-Agent": DISCORD_USER_AGENT,
            },
        );
    } catch (e) {
        log_warn("discord: DM channel create failed for user " + user_id + ": " + e);
        return ();
    }
    if resp == () || resp.id == () { return (); }
    // Cache.
    let m = #{};
    if cache_raw != () && cache_raw != "" {
        try { m = parse_json(cache_raw); } catch (e) { m = #{}; }
        if type_of(m) != "map" { m = #{}; }
    }
    m[user_id] = resp.id;
    vault_put("dm_channels", to_json_string(m));
    resp.id
}

fn load_bot_user() {
    // vault_get can throw if host_caps isn't wired (test
    // fixtures) — treat that the same as "no row" so the
    // decoder + tool dispatch stay defensible without a live
    // host stack. Same pattern used by lookup_channel_name and
    // cache_reply_target below.
    let raw = ();
    try { raw = vault_get("bot_user"); } catch (e) { return (); }
    if raw == () || raw == "" { return (); }
    try { return parse_json(raw); } catch (e) { return (); }
}

fn lookup_channel_name(guild_id, channel_id) {
    let raw = ();
    try { raw = vault_get("guilds"); } catch (e) { return (); }
    if raw == () || raw == "" { return (); }
    try {
        let m = parse_json(raw);
        if type_of(m) != "map" { return (); }
        let g = m[guild_id];
        if g == () || g.channels == () { return (); }
        for c in g.channels {
            if c.id == channel_id { return "#" + c.name; }
        }
    } catch (e) {}
    ()
}

fn cache_reply_target(channel_id, message_id) {
    if channel_id == () || message_id == () { return; }
    let raw = vault_get("last_dispatch_message");
    let m = #{};
    if raw != () && raw != "" {
        try { m = parse_json(raw); } catch (e) { m = #{}; }
        if type_of(m) != "map" { m = #{}; }
    }
    m[channel_id] = message_id;
    vault_put("last_dispatch_message", to_json_string(m));
}

fn lookup_reply_target(channel_id) {
    let raw = vault_get("last_dispatch_message");
    if raw == () || raw == "" { return (); }
    try {
        let m = parse_json(raw);
        if type_of(m) == "map" && m[channel_id] != () { return m[channel_id]; }
    } catch (e) {}
    ()
}

// Replace `<@bot-id>` and `<@!bot-id>` mention tokens with
// "@execlaw" so the agent sees a clean prompt rather than
// "<@1234567890> what's the weather?".
fn strip_bot_mentions(text, bot_id) {
    if text == () || text == "" { return ""; }
    let s = text;
    let a = "<@" + bot_id + ">";
    let b = "<@!" + bot_id + ">";
    // Rhai's .replace mutates in place + returns unit. We have
    // to manually scan/rebuild since we want the result back.
    s.replace(a, "@execlaw");
    s.replace(b, "@execlaw");
    s
}

// Parse Discord's ISO-8601 timestamp (e.g. "2026-05-10T12:34:56.789000+00:00")
// into epoch-milliseconds. Discord always emits UTC; the offset
// is always +00:00 so we don't need a real timezone parser.
fn parse_discord_timestamp_ms(ts) {
    if ts == () || ts == "" { return (); }
    // Quick-and-dirty: split on '.' or '+', take the "YYYY-MM-DDTHH:MM:SS" head.
    let dot = ts.index_of(".");
    let plus = ts.index_of("+");
    let cut = if dot > 0 && (plus < 0 || dot < plus) { dot }
              else if plus > 0 { plus }
              else { ts.len() };
    let head = ts.sub_string(0, cut);
    if head.len() != 19 { return (); }
    let year   = host_parse_int(head.sub_string(0, 4));
    let month  = host_parse_int(head.sub_string(5, 2));
    let day    = host_parse_int(head.sub_string(8, 2));
    let hour   = host_parse_int(head.sub_string(11, 2));
    let minute = host_parse_int(head.sub_string(14, 2));
    let second = host_parse_int(head.sub_string(17, 2));
    if year == () || month == () || day == () || hour == () || minute == () || second == () {
        return ();
    }
    // Civil date → days since epoch (1970-01-01) via Howard
    // Hinnant's date algorithm. Works for any year 1900+.
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 9 } else { month - 3 };
    let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
    let yoe = y - era * 400;                              // [0, 399]
    let doy = (153 * m + 2) / 5 + day - 1;                // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;      // [0, 146096]
    let days = era * 146097 + doe - 719468;
    let seconds = days * 86400 + hour * 3600 + minute * 60 + second;
    // Add fractional milliseconds if present.
    let ms_extra = 0;
    if dot > 0 {
        let after = ts.sub_string(dot + 1);
        let end = after.index_of("+");
        let frac = if end > 0 { after.sub_string(0, end) } else { after };
        // First 3 digits → milliseconds.
        if frac.len() >= 3 {
            let p = host_parse_int(frac.sub_string(0, 3));
            if p != () { ms_extra = p; }
        } else if frac.len() > 0 {
            let p = host_parse_int(frac);
            if p != () { ms_extra = p; }
        }
    }
    seconds * 1000 + ms_extra
}

// Smart paragraph/line/space-aware split at MAX_DISCORD_MESSAGE_CHARS.
// Mirrors selfhosted-claw's splitSlackMessage shape — same logic
// works for any text-flow channel.
fn split_discord_message(text) {
    let trimmed = trim(text);
    if trimmed == "" { return []; }
    if trimmed.len() <= 1900 { return [trimmed]; }
    let out = [];
    let remaining = trimmed;
    while remaining.len() > 1900 {
        let cap = 1900;
        let lo = cap / 2;
        let head = remaining.sub_string(0, cap);
        let lastpara = last_index_of_in(head, "\n\n");
        let lastline = last_index_of_in(head, "\n");
        let lastspace = last_index_of_in(head, " ");
        let cut = if lastpara >= lo { lastpara }
            else if lastline >= lo { lastline }
            else if lastspace >= lo { lastspace }
            else { cap };
        out.push(trim(remaining.sub_string(0, cut)));
        remaining = trim(remaining.sub_string(cut));
    }
    if remaining != "" { out.push(remaining); }
    out
}

fn last_index_of_in(s, needle) {
    let i = s.len();
    let nlen = needle.len();
    if nlen == 0 || i < nlen { return -1; }
    let pos = i - nlen;
    while pos >= 0 {
        if s.sub_string(pos, nlen) == needle { return pos; }
        if pos == 0 { return -1; }
        pos -= 1;
    }
    -1
}

// =====================================================================
// ADMIN ROUTES
// =====================================================================

fn admin_status(req) {
    let token = vault_get("bot_token");
    let configured = token != () && token != "";
    let bot_user = load_bot_user();
    let guilds_raw = vault_get("guilds");
    let guild_count = 0;
    if guilds_raw != () && guilds_raw != "" {
        try {
            let m = parse_json(guilds_raw);
            if type_of(m) == "map" { guild_count = m.keys().len(); }
        } catch (e) {}
    }
    #{
        "sidecar_status": if configured { "healthy" } else { "unconfigured" },
        "sidecar_rpc_url": (),
        "registered_accounts": [],
        "accounts_on_disk": [],
        "fetch_error": (),
        "bot_user_id": if bot_user != () && bot_user.id != () { bot_user.id } else { () },
        "bot_username": if bot_user != () && bot_user.username != () { bot_user.username } else { () },
        "guilds_known": guild_count,
        "token_masked": mask(token),
    }
}

fn admin_get_config(req) {
    let token = vault_get("bot_token");
    let bot_user = load_bot_user();
    #{
        "bot_token_masked": mask(token),
        "configured": token != () && token != "",
        "bot_user_id": if bot_user != () && bot_user.id != () { bot_user.id } else { () },
        "bot_username": if bot_user != () && bot_user.username != () { bot_user.username } else { () },
    }
}

// POST /config  body: {bot_token}
//   * Validates via GET /users/@me. Bad token → 401 → throw.
//   * Persists to vault on success.
//   * Hot-reloads: closes the previous bidi WS handle and re-runs
//     on_enable so the new token takes effect without an operator
//     disable / re-enable cycle.
fn admin_set_config(req) {
    if req.body == () { throw "missing request body"; }
    let token = if req.body.bot_token != () { trim(req.body.bot_token) } else { "" };
    if token == "" { throw "bot_token required"; }
    // Discord bot tokens have no fixed prefix in v10+ (the old
    // "Bot " prefix is the Authorization header, not the token);
    // we don't try to validate format beyond non-empty.
    let me = ();
    try {
        me = http_get(DISCORD_API + "/users/@me", #{}, "", #{
            "Authorization": "Bot " + token,
            "User-Agent": DISCORD_USER_AGENT,
        });
    } catch (e) {
        throw "Discord rejected the bot_token: " + e;
    }
    if me == () || me.id == () {
        throw "/users/@me returned no id; token is probably invalid";
    }
    // Persist on success.
    vault_put("bot_token", token);
    let cached = #{};
    cached.id = me.id;
    if me.username != () { cached.username = me.username; }
    if me.application_id != () { cached.application_id = me.application_id; }
    else { cached.application_id = me.id; }
    vault_put("bot_user", to_json_string(cached));

    // Hot-reload — close any active subscription + re-enable. The
    // ws_subscribe_bidi binding closes the previous active handle
    // on re-subscribe (commit aa04700) so this is belt-and-braces.
    ws_close_active();
    on_enable();

    #{
        "ok": true,
        "bot_user_id": me.id,
        "bot_username": me.username,
    }
}

fn admin_test_message(req) {
    if req.body == () { throw "missing request body"; }
    let channel_id = if req.body.channel_id != () { req.body.channel_id } else { "" };
    if channel_id == "" { throw "channel_id required"; }
    let token = vault_get("bot_token");
    if token == () || token == "" { throw "no bot_token configured"; }
    let resp = http_post(
        DISCORD_API + "/channels/" + channel_id + "/messages",
        #{
            "content": "execlaw test message — Discord plugin v0.1.0 wiring confirmed.",
            "allowed_mentions": #{ "parse": ["users"] },
        },
        "",
        #{
            "Authorization": "Bot " + token,
            "User-Agent": DISCORD_USER_AGENT,
        },
    );
    if resp == () || resp.id == () {
        return #{ "ok": false, "error": "send failed" };
    }
    #{ "ok": true, "message_id": resp.id }
}

fn mask(s) {
    if s == () || s == "" { return ""; }
    let n = s.len();
    if n <= 4 { return repeat_char("*", n); }
    repeat_char("*", n - 4) + s.sub_string(n - 4)
}

fn repeat_char(ch, n) {
    let out = "";
    let i = 0;
    while i < n {
        out += ch;
        i += 1;
    }
    out
}

// =====================================================================
// TEST HOOK — invoked from crates/script/tests/discord_plugin.rs
// =====================================================================

// Wrap decode_message_create with a String-returning shim so tests
// can drive the decoder over a captured wire payload without
// having to recreate the env-level dispatch machinery.
fn _test_decode_message_create(raw, bot_id) {
    let d = parse_json(raw);
    let bot_user = if bot_id != "" { #{ "id": bot_id, "username": "execlaw" } } else { () };
    let inbound = decode_message_create(d, bot_user);
    to_json_string(inbound)
}

fn _test_compute_heartbeat() {
    compute_heartbeat()
}

fn _test_split(text) {
    to_json_string(split_discord_message(text))
}

fn _test_parse_timestamp(ts) {
    parse_discord_timestamp_ms(ts)
}
