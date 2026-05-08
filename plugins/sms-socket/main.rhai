// sms-socket — SMS / MMS via the Android SMS Socket app's local WebSocket.
//
// Mirrors Slack's "no-sidecar, just open a WebSocket" shape:
// op pastes API key + gateway URL → plugin opens a bidirectional
// WS to the Android phone running sms-socket-app → inbound events
// fan out to host_route_inbound, outbound tool calls submit
// envelopes back over the same socket.
//
// Wire format (https://github.com/crockpotveggies/sms-socket-app):
//
// Outbound envelopes plugin → gateway:
//   { type: "sendSms",          requestId, payload: { destination, body, subscriptionId? } }
//   { type: "sendMms",          requestId, payload: { destination, body?, attachment, subscriptionId? } }
//   { type: "rehydrate",        requestId, payload: { since, limit } }
//   { type: "getGatewayState",  requestId, payload: {} }
//
// Inbound envelopes gateway → plugin:
//   { type: "response", requestId, ok, payload }   — request acks
//   { type: "sms.received",       payload: { address, body, receivedAt, messageId } }
//   { type: "sms.outbound.sent",  payload: { destination, body, sentAt, messageId } }
//   { type: "mms.received",       payload: { address, body?, attachments[], receivedAt } }
//   { type: "mms.outbound.sent",  payload: { destination, body?, attachments[], sentAt } }
//   { type: "gateway.state",      payload: { running, enabled, addresses[], connectionCount, apiKeyPreview } }
//
// v1 simplifications vs the selfhosted-claw integration:
//   * Fire-and-forget outbound — we don't track requestId →
//     waiter promises (Rhai's per-call scope makes this awkward).
//     Errors surface via the next response frame, which we log
//     but don't pipe back to the agent. Followup if this matters.
//   * No history rehydrate cursor — we don't persist
//     lastSeenTimestamp across reboots in v1. The gateway will
//     re-deliver recent events on every reconnect; the host's
//     dedup on (channel, native_id, timestamp_ms, text) catches
//     the duplicates.
//   * MMS attachment ingest pulls the embedded base64 into the
//     host's attachment store via the standard inbound flow.
//   * No per-SIM subscription routing through the agent — the
//     operator's configured `default_subscription_id` is applied
//     to every send.
//   * Outbound sends use `ws_send_to_active(json)` — the host
//     keeps a per-plugin "current active bidi WS handle" slot
//     populated by `ws_subscribe_bidi`. Tool calls reach the
//     socket without threading the handle through per-call engine
//     scope. Concurrent sends are safe (the handle's mpsc outbox
//     is lock-free for producers).

const DEFAULT_GATEWAY_URL = "ws://127.0.0.1:8787/";
const REHYDRATE_LIMIT = 200;

// =====================================================================
// LIFECYCLE
// =====================================================================

fn on_enable() {
    let api_key = vault_get("api_key");
    if api_key == () || api_key == "" {
        log_info("sms-socket: no api_key in vault — visit Settings → Plugins → SMS Socket to configure");
        return;
    }
    let url = vault_get("gateway_url");
    if url == () || url == "" {
        url = "ws://127.0.0.1:8787/";
    }
    if !url.starts_with("ws://") && !url.starts_with("wss://") {
        log_warn("sms-socket: gateway_url must start with ws:// or wss:// (got '" + url + "')");
        return;
    }
    log_info("sms-socket: subscribing to gateway at " + url);
    let headers = #{ "Authorization": "Bearer " + api_key };

    // Handshake the gateway expects on every connect. Without
    // these, the gateway treats us as a passive listener and never
    // delivers `gateway.state` pings or live `sms.received` events.
    // Mirrors selfhosted-claw's onopen sequence
    // (sms-socket.ts:430-457).
    //
    //   1. getGatewayState — registers this connection as an
    //      active subscriber and prompts the gateway to start
    //      pushing live events to us.
    //   2. rehydrate       — replays history we missed while
    //      disconnected. The `since` cursor is critical: with
    //      since=0 the gateway returns every buffered event going
    //      back potentially weeks, and we'd flood the agent with
    //      historical messages, which in turn would fire 40+
    //      outbound auto-replies. Track the most-recent received
    //      event timestamp in vault and pass that as `since`.
    //
    // First-install case: no `last_seen_ms` in the vault yet.
    // Default to NOW so we don't dump pre-install history through
    // the agent. The trade-off is that any SMS that came in during
    // the few seconds between install and on_enable would be
    // missed on the very first connect — acceptable; live delivery
    // takes over from this point.
    let last_seen_ms = vault_get("last_seen_ms");
    let since;
    if last_seen_ms != () && last_seen_ms != "" {
        try {
            since = last_seen_ms.to_int();
        } catch (e) {
            since = now() * 1000;
        }
    } else {
        since = now() * 1000;
        vault_put("last_seen_ms", since.to_string());
        log_info("sms-socket: first connect — anchoring rehydrate cursor at " + since + "ms");
    }

    let init_state = #{
        "type": "getGatewayState",
        "requestId": "sms-handshake-state",
        "payload": #{},
    };
    let init_rehydrate = #{
        "type": "rehydrate",
        "requestId": "sms-handshake-rehydrate",
        "payload": #{
            "since": since,
            "limit": 200,
        },
    };
    let init_frames = [
        to_json_string(init_state),
        to_json_string(init_rehydrate),
    ];
    let _handle = ws_subscribe_bidi(url, "on_frame", headers, init_frames);
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
        log_warn("sms-socket: parse_json failed: " + e);
        return;
    }
    if env == () { return; }

    let env_type = env.type;
    if env_type == () || env_type == "" { return; }

    if env_type == "response" {
        // Outbound request ack. v1 fire-and-forget — log non-ok
        // responses so the operator sees server-side errors, but
        // don't propagate to the agent (it already returned).
        if env.ok != true {
            let detail = ();
            if env.payload != () && env.payload.error != () {
                detail = env.payload.error;
            }
            log_warn("sms-socket: gateway rejected requestId=" + env.requestId + ": " + detail);
            return;
        }
        // Rehydrate response carries a history of events in
        // payload.events[]. Each is a full envelope (type, payload).
        //
        // Defense-in-depth cursor filter: even though we asked
        // rehydrate `since: last_seen_ms`, defend against gateway
        // bugs that ignore the cursor and dump full history. We
        // skip any event with a timestamp <= the cursor we PASSED
        // IN. The cursor was captured at on_enable time, BEFORE
        // any new events arrived in this connection, so:
        //
        //   * Events older than the cursor → already processed in
        //     a past session, drop them (no agent dispatch).
        //   * Events newer than the cursor → genuinely missed
        //     while disconnected; route them.
        //
        // Without this filter, a fresh install or a gateway that
        // returns the full buffer floods the agent with backlog
        // and triggers 40+ outbound auto-replies.
        if env.requestId == "sms-handshake-rehydrate"
            && env.payload != ()
            && env.payload.events != ()
            && type_of(env.payload.events) == "array"
        {
            let count = env.payload.events.len();
            // Re-read the cursor at replay time. Using the same
            // value we passed as `since` keeps the replay window
            // closed under the rehydrate semantics.
            let cursor = read_last_seen_ms();
            log_info("sms-socket: rehydrate returned " + count + " event(s); cursor=" + cursor + "ms");
            let routed = 0;
            let skipped = 0;
            for nested in env.payload.events {
                if type_of(nested) != "map" { continue; }
                let nested_type = nested.type;
                if nested_type == () || nested_type == "" { continue; }
                if nested_type != "sms.received" && nested_type != "mms.received" {
                    continue;
                }
                let nested_ts = ();
                if nested.payload != () && nested.payload.receivedAt != () {
                    nested_ts = nested.payload.receivedAt;
                } else if nested.timestamp != () {
                    nested_ts = nested.timestamp;
                }
                if nested_ts != () && nested_ts <= cursor {
                    skipped += 1;
                    continue;
                }
                if route_inbound_event(nested_type, nested) {
                    routed += 1;
                }
            }
            log_info("sms-socket: rehydrate replay done; routed=" + routed + " skipped=" + skipped);
        }
        return;
    }

    if env_type == "gateway.state" {
        // Periodic state ping — cache in vault so the SPA's
        // /status reads can surface it without a fresh request.
        if env.payload != () {
            try {
                vault_put("gateway_state", to_json_string(env.payload));
            } catch (e) {}
        }
        return;
    }

    // Filter to events we route through the host pipeline.
    // Outbound-self events (sms.outbound.sent, mms.outbound.*) are
    // dropped — we already wrote the user_msg / model_turn when
    // the agent sent.
    if env_type != "sms.received" && env_type != "mms.received" {
        return;
    }

    route_inbound_event(env_type, env);
}

// Single dispatch path used by both live frames + rehydrate replay.
// Updates last_seen_ms in vault on every routed event so the next
// rehydrate's `since` cursor reflects what we've actually consumed.
// Returns true on successful dispatch, false on decode/skip.
fn route_inbound_event(env_type, env) {
    let inbound = decode_event(env_type, env);
    if inbound == () { return false; }
    // Persist the cursor BEFORE dispatching, not after. If the
    // host dispatch throws (e.g. UNIQUE constraint on duplicate
    // ingest), we still want the cursor moved past this event so
    // the NEXT rehydrate doesn't re-fetch it. The host's own dedup
    // is a safety net; the cursor is the primary correctness lever.
    let ts = inbound.timestamp_ms;
    if ts != () {
        let prev = read_last_seen_ms();
        if ts > prev {
            vault_put("last_seen_ms", ts.to_string());
        }
    }
    try {
        host_route_inbound(inbound);
    } catch (e) {
        log_warn("sms-socket: host_route_inbound rejected: " + e);
    }
    true
}

fn read_last_seen_ms() {
    let raw = vault_get("last_seen_ms");
    if raw == () || raw == "" { return 0; }
    try {
        return raw.to_int();
    } catch (e) {
        return 0;
    }
}

fn decode_event(env_type, env) {
    try {
        let payload = env.payload;
        if payload == () { return (); }

        // Sender phone: gateway uses `address` for inbound (the
        // contact's number), `destination` for outbound. Inbound-
        // only path so just read `address`.
        let phone = if payload.address != () { trim(payload.address) } else { "" };
        if phone == "" { return (); }

        let text = if payload.body != () { trim(payload.body) } else { "" };

        // MMS may include attachments. Surface them as
        // `bridge_id` = the gateway's per-attachment id; the host
        // pulls the bytes via the standard inbound attachment
        // ingest path. base64 is also embedded in the payload —
        // we don't decode it here; that's the host's job.
        let attachments = [];
        if payload.attachments != () && type_of(payload.attachments) == "array" {
            for a in payload.attachments {
                let bridge_id = if a.id != () { a.id } else if a.fileName != () { a.fileName } else { () };
                if bridge_id == () || bridge_id == "" { continue; }
                attachments.push(#{
                    "bridge_id": bridge_id,
                    "content_type": a.mimeType,
                    "filename": a.fileName,
                    "size_bytes": a.sizeBytes,
                });
            }
        }

        // Drop empty messages with no attachments.
        if text == "" && attachments.is_empty() { return (); }

        // Timestamp: gateway sends `receivedAt` as Unix ms.
        let ts_ms = ();
        if payload.receivedAt != () {
            ts_ms = payload.receivedAt;
        } else if env.timestamp != () {
            ts_ms = env.timestamp;
        }

        return #{
            "channel": "sms",
            "native_id": phone,
            "display_name": (), // SMS doesn't carry a name
            "group_id": (),     // 1:1 only — SMS has no group concept here
            "group_name": (),
            "text": text,
            "timestamp_ms": ts_ms,
            "attachments": attachments,
        };
    } catch (e) {
        log_warn("sms-socket: undecodable event: " + e);
        return ();
    }
}

// =====================================================================
// TOOL DISPATCH
// =====================================================================

fn tool_call(name, args, oauth) {
    if name == "sms.send_message"           { return send_message_impl(args); }
    if name == "sms.reply"                  { return reply_impl(args); }
    if name == "sms.send_with_attachments"  { return send_with_attachments_impl(args); }
    if name == "sms.set_typing"             { return set_typing_impl(args); }
    if name == "sms.fetch_attachment"       { return fetch_attachment_impl(args); }
    throw "unknown tool '" + name + "'";
}

fn send_message_impl(args) {
    let to = args.to;
    let text = args.text;
    if to == () || to == "" { throw "sms.send_message requires `to`"; }
    if text == () || text == "" { throw "sms.send_message requires `text`"; }
    let destination = strip_sms_prefix(to);
    validate_e164(destination);
    let req_id = next_request_id();
    let envelope = #{
        "type": "sendSms",
        "requestId": req_id,
        "payload": with_subscription(#{
            "destination": destination,
            "body": text,
        }),
    };
    submit_envelope(envelope, "sms.send_message");
    "ok"
}

fn reply_impl(args) {
    if args.to == () || args.to == "" {
        throw "sms.reply requires `to` — host fills this from the conversation's binding";
    }
    send_message_impl(args)
}

fn send_with_attachments_impl(args) {
    let to = args.to;
    let text = if args.text != () { args.text } else { "" };
    let attachments = args.attachments;
    if to == () || to == "" { throw "sms.send_with_attachments requires `to`"; }
    let destination = strip_sms_prefix(to);
    validate_e164(destination);

    // No attachments → fall through to plain SMS.
    if attachments == () || type_of(attachments) != "array" || attachments.is_empty() {
        return send_message_impl(#{ "to": to, "text": text });
    }
    if attachments.len() > 4 {
        throw "sms.send_with_attachments: max 4 MMS attachments per send; got " + attachments.len();
    }

    // Each attachment is its own sendMms request — the gateway
    // accepts one attachment per send. Caption rides on the
    // first send only.
    let i = 0;
    for aid in attachments {
        let a = host_get_attachment_bytes(aid);
        // host_get_attachment_bytes returns {data_url, mime_type, size_bytes}
        // The gateway wants raw base64 (no data: prefix). Strip it.
        let b64 = strip_data_url_prefix(a.data_url);
        let filename = "attachment-" + (i + 1);
        if a.filename != () && a.filename != "" { filename = a.filename; }
        let req_id = next_request_id();
        let envelope = #{
            "type": "sendMms",
            "requestId": req_id,
            "payload": with_subscription(#{
                "destination": destination,
                "body": if i == 0 && text != "" { text } else { () },
                "attachment": #{
                    "fileName": filename,
                    "mimeType": a.mime_type,
                    "sizeBytes": a.size_bytes,
                    "base64": b64,
                },
            }),
        };
        submit_envelope(envelope, "sms.send_with_attachments");
        i += 1;
    }
    "ok"
}

fn set_typing_impl(args) {
    // SMS has no typing indicator. Return ok silently so the
    // host's typing-guard doesn't error.
    if args == () { return #{ "ok": true }; }
    #{ "ok": true }
}

fn fetch_attachment_impl(args) {
    throw "sms.fetch_attachment: not implemented in v0.1 — MMS attachments arrive embedded in the inbound event payload, not via a separate fetch endpoint";
}

// =====================================================================
// HELPERS
// =====================================================================

fn strip_sms_prefix(s) {
    if s == () || s == "" { return s; }
    if s.starts_with("sms:") { return s.sub_string(4); }
    s
}

fn validate_e164(phone) {
    if phone == () || phone == "" { throw "phone number is empty"; }
    if !phone.starts_with("+") { throw "phone must be E.164 (start with '+'): got '" + phone + "'"; }
    let digits = phone.sub_string(1);
    let n = digits.len();
    if n < 7 || n > 15 { throw "phone has wrong digit count for E.164: '" + phone + "'"; }
}

fn strip_data_url_prefix(s) {
    // host_get_attachment_bytes returns "data:<mime>;base64,<payload>".
    // The gateway wants just the payload.
    if s == () || s == "" { return s; }
    let comma = s.index_of(",");
    if comma < 0 { return s; }
    s.sub_string(comma + 1)
}

fn with_subscription(payload) {
    // admin_set_config validates that default_subscription_id is
    // an integer at write time, so we don't need a runtime catch
    // here — if the vault has anything else, the install is
    // already corrupt and a thrown error is the right surface.
    let sub = vault_get("default_subscription_id");
    if sub != () && sub != "" {
        payload.subscriptionId = sub.to_int();
    }
    payload
}

fn next_request_id() {
    // Tag with a unix-second timestamp. v1 is fire-and-forget so
    // we don't actually correlate response → originating tool
    // call; collisions on the same second are logging-only noise.
    //
    // Why not a vault counter? Two concurrent tool calls would
    // both read the same value and double-mint (RMW isn't atomic
    // across calls). The gateway only correlates within a
    // connection so a "good enough" tag is fine.
    "sms-" + now().to_string()
}

// Submit an outbound envelope over the plugin's active bidi WS.
// `ws_send_to_active` reaches into the host-managed per-plugin
// slot populated by ws_subscribe_bidi — no per-call engine state
// needed, no vault RMW, safe under concurrent tool-call producers
// (the handle's outbox is an mpsc).
//
// Returns "ok" on enqueue success; throws on disconnect so the
// agent sees the failure rather than silent black-hole. The
// gateway's protocol has no client-side redelivery, so we don't
// retry — if the socket is down, the operator's connection is
// hosed and queueing won't fix it.
fn submit_envelope(envelope, tool_name) {
    let json_str = to_json_string(envelope);
    let ok = ws_send_to_active(json_str);
    if !ok {
        throw "sms-socket: " + tool_name + " failed — WebSocket not connected. " +
              "Check the plugin is enabled and the gateway URL is reachable " +
              "(Settings → Plugins → SMS Socket → Status).";
    }
    log_info("sms-socket: sent " + tool_name + " envelope #" + envelope.requestId);
}

// =====================================================================
// ADMIN ROUTES
// =====================================================================

fn admin_status(req) {
    let api_key = vault_get("api_key");
    let configured = api_key != () && api_key != "";
    let gateway_url = vault_get("gateway_url");
    if gateway_url == () || gateway_url == "" {
        gateway_url = "ws://127.0.0.1:8787/";
    }

    // Surface the most recent gateway.state ping if any.
    let gateway_state = ();
    let raw = vault_get("gateway_state");
    if raw != () && raw != "" {
        try {
            gateway_state = parse_json(raw);
        } catch (e) {}
    }

    #{
        "sidecar_status": if configured { "configured" } else { "unconfigured" },
        "sidecar_rpc_url": (),
        "gateway_url": gateway_url,
        "configured": configured,
        "gateway_state": gateway_state,
    }
}

fn admin_get_config(req) {
    let api_key = vault_get("api_key");
    let gateway_url = vault_get("gateway_url");
    let sub = vault_get("default_subscription_id");
    #{
        "api_key_set": api_key != () && api_key != "",
        "api_key_masked": mask(api_key),
        "gateway_url": if gateway_url != () && gateway_url != "" { gateway_url } else { "ws://127.0.0.1:8787/" },
        "default_subscription_id": if sub != () { sub } else { "" },
    }
}

fn admin_set_config(req) {
    if req.body == () { throw "missing request body"; }
    let raw_key = if req.body.api_key != () { req.body.api_key } else { "" };
    let raw_url = if req.body.gateway_url != () { req.body.gateway_url } else { "" };
    let raw_sub = if req.body.default_subscription_id != () { req.body.default_subscription_id } else { "" };
    let api_key = trim(raw_key);
    let gateway_url = trim(raw_url);
    let sub = trim(raw_sub);

    // Pre-validate before mutating anything in the vault — half-
    // applied config (new url written, ws:// check throws) leaves
    // the plugin in a confusing state. Validate first, then write.
    if gateway_url != ""
        && !gateway_url.starts_with("ws://")
        && !gateway_url.starts_with("wss://")
    {
        throw "gateway_url must start with ws:// or wss://";
    }
    if sub != "" {
        try {
            sub.to_int();
        } catch (e) {
            throw "default_subscription_id must be an integer";
        }
    }

    if api_key == "" {
        vault_delete("api_key");
    } else {
        vault_put("api_key", api_key);
    }
    if gateway_url == "" {
        vault_delete("gateway_url");
    } else {
        vault_put("gateway_url", gateway_url);
    }
    if sub == "" {
        vault_delete("default_subscription_id");
    } else {
        vault_put("default_subscription_id", sub);
    }

    // Reconnect in place so the operator doesn't have to bounce
    // the plugin to pick up new credentials. ws_close_active()
    // cancels the existing WS (if any) cooperatively — the
    // consumer task drops on the next loop tick. on_enable() then
    // opens a fresh subscription with the values we just wrote.
    //
    // Both calls are no-ops in the obvious edge cases:
    //   * ws_close_active() returns false when there's no active
    //     handle (first save before anything was running).
    //   * on_enable() returns early when api_key is missing /
    //     gateway_url is malformed, logging a warn — exactly the
    //     same as the cold-start path.
    let was_active = ws_close_active();
    on_enable();
    log_info(
        "sms-socket: config updated; "
            + (if was_active { "old WS torn down, " } else { "" })
            + "reconnected with new credentials"
    );
    #{ "ok": true, "reconnected": true }
}

fn admin_test_message(req) {
    if req.body == () { throw "missing request body"; }
    let to = if req.body.to != () { trim(req.body.to) } else { "" };
    if to == "" { throw "to required"; }
    let destination = strip_sms_prefix(to);
    validate_e164(destination);
    let req_id = next_request_id();
    let envelope = #{
        "type": "sendSms",
        "requestId": req_id,
        "payload": with_subscription(#{
            "destination": destination,
            "body": "execlaw test message — SMS Socket plugin v0.1.0 wiring confirmed.",
        }),
    };
    submit_envelope(envelope, "sms.admin_test");
    #{ "ok": true, "request_id": req_id, "note": "Sent on the active gateway WebSocket." }
}

fn mask(s) {
    if s == () || s == "" { return ""; }
    let n = s.len();
    let dots = repeat_char("*", if n <= 4 { n } else { n - 4 });
    if n <= 4 { return dots; }
    dots + s.sub_string(n - 4)
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
// TEST-ONLY HELPERS
// =====================================================================
//
// `_test_decode(raw_json)` — wraps parse_json + decode_event in a
// single string-arg call so the unit-test fixture in
// `crates/script/tests/sms_socket_plugin.rs` doesn't have to
// hand-construct a Rhai Map across the FFI boundary.
//
// Production code paths go through on_frame, never through this
// helper. The leading underscore is a convention signal — any
// function whose name starts with `_test_` should be assumed
// non-load-bearing.
fn _test_decode(raw) {
    let env;
    try { env = parse_json(raw); } catch (e) { return (); }
    if env == () { return (); }
    let env_type = env.type;
    if env_type == () || env_type == "" { return (); }
    decode_event(env_type, env)
}
