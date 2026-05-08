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
    let _handle = ws_subscribe_bidi(url, "on_frame", headers);
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

    // Drain any queued outbound envelopes over the now-active
    // socket. See submit_envelope for why this lives here instead
    // of inline in the tool calls.
    try {
        flush_outbox(handle);
    } catch (e) {
        log_warn("sms-socket: flush_outbox failed: " + e);
    }

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

    let inbound = decode_event(env_type, env);
    if inbound == () { return; }
    try {
        host_route_inbound(inbound);
    } catch (e) {
        log_warn("sms-socket: host_route_inbound rejected: " + e);
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
    let sub = vault_get("default_subscription_id");
    if sub != () && sub != "" {
        try {
            payload.subscriptionId = sub.to_int();
        } catch (e) {
            // Non-integer in vault — ignore.
        }
    }
    payload
}

fn next_request_id() {
    // Monotonic-ish — we don't persist across plugin reloads but
    // the gateway only correlates within a connection so this
    // suffices for v1. Tag-prefixed so debugging logs are clear.
    let raw = vault_get("req_counter");
    let n = 0;
    if raw != () && raw != "" {
        try { n = raw.to_int(); } catch (e) { n = 0; }
    }
    n += 1;
    vault_put("req_counter", n.to_string());
    "sms-" + n.to_string()
}

// Submit an outbound envelope over the active WS handle. Stashed
// on every connect by stash_handle below; tool calls (which run
// in their own Rhai engine instance) read it back.
//
// LIMITATION: the WsSubscriptionHandle is held by the on_enable
// closure, NOT in the per-call engine scope. Tool calls run in a
// fresh scope so they can't reach `_handle` directly. v1
// workaround: cache the URL in vault and reconnect-or-reuse via
// a side-channel. ACTUALLY simpler — the WS consumer in the host
// keeps the handle alive (closure holds it), and outbound writes
// must go through ws_send which needs the handle. So we have to
// stash the handle somehow accessible to the tool call.
//
// For v0.1: tool calls cannot directly send. Instead we queue
// envelopes via a vault-backed outbox that on_frame drains on
// the NEXT inbound event. This adds latency (next event needed
// to flush) but works without per-engine state.
//
// This is uglier than I'd like — a follow-up should expose a
// "current ws handle" engine-level slot. For now, accept the
// flush-on-next-frame quirk: SMS conversations have natural
// back-and-forth, the operator's reply will trigger the flush.
fn submit_envelope(envelope, tool_name) {
    let outbox_raw = vault_get("outbox");
    let outbox = [];
    if outbox_raw != () && outbox_raw != "" {
        try {
            let parsed = parse_json(outbox_raw);
            if type_of(parsed) == "array" { outbox = parsed; }
        } catch (e) {}
    }
    outbox.push(envelope);
    vault_put("outbox", to_json_string(outbox));
    log_info("sms-socket: queued " + tool_name + " envelope #" + envelope.requestId + " (will flush on next inbound frame)");
}

// Drain the vault-backed outbox over the active WS handle.
// Called from on_frame on every inbound event — typical SMS
// back-and-forth flushes near-instantly because the gateway
// emits gateway.state pings + outbound-sent receipts that
// trigger this.
fn flush_outbox(handle) {
    let raw = vault_get("outbox");
    if raw == () || raw == "" { return; }
    let outbox;
    try {
        outbox = parse_json(raw);
    } catch (e) { return; }
    if type_of(outbox) != "array" || outbox.is_empty() { return; }
    let remaining = [];
    for env in outbox {
        let json_str = to_json_string(env);
        let ok = ws_send(handle, json_str);
        if !ok {
            // Disconnected — keep in queue. Next frame's flush
            // will retry once the consumer reconnects.
            remaining.push(env);
        }
    }
    if remaining.is_empty() {
        vault_delete("outbox");
    } else {
        vault_put("outbox", to_json_string(remaining));
    }
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

    let outbox_pending = 0;
    let outbox_raw = vault_get("outbox");
    if outbox_raw != () && outbox_raw != "" {
        try {
            let parsed = parse_json(outbox_raw);
            if type_of(parsed) == "array" { outbox_pending = parsed.len(); }
        } catch (e) {}
    }

    #{
        "sidecar_status": if configured { "configured" } else { "unconfigured" },
        "sidecar_rpc_url": (),
        "gateway_url": gateway_url,
        "configured": configured,
        "gateway_state": gateway_state,
        "outbox_pending": outbox_pending,
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

    if api_key == "" {
        vault_delete("api_key");
    } else {
        vault_put("api_key", api_key);
    }
    if gateway_url == "" {
        vault_delete("gateway_url");
    } else {
        if !gateway_url.starts_with("ws://") && !gateway_url.starts_with("wss://") {
            throw "gateway_url must start with ws:// or wss://";
        }
        vault_put("gateway_url", gateway_url);
    }
    if sub == "" {
        vault_delete("default_subscription_id");
    } else {
        // Validate it's an integer.
        try {
            sub.to_int();
        } catch (e) {
            throw "default_subscription_id must be an integer";
        }
        vault_put("default_subscription_id", sub);
    }
    log_info("sms-socket: config updated");
    #{ "ok": true, "restart_required": "Restart the plugin (Settings → Plugins → SMS Socket → disable + enable) to reconnect with the new credentials." }
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
    #{ "ok": true, "request_id": req_id, "note": "Queued. Will flush on next inbound gateway frame (gateway.state pings every ~5s)." }
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
