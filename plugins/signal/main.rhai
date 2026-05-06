// signal — Signal Messenger transport, fully script-tier (Phase B).
//
// EVERYTHING that used to live in crates/server/src/signal_*.rs is
// now here. The host owns:
//   * Trust admit + conversation routing (host_route_inbound).
//   * Group-address LLM classifier.
//   * Auto-bridge of agent text replies (calls signal.send_message).
//   * Sidecar supervisor (spawns the bbernhard container).
//
// This plugin owns:
//   * WebSocket inbound consumer (on_enable -> ws_subscribe).
//   * Frame decoder (on_frame -> host_route_inbound).
//   * All signal.* tools (send_message, reply, list_groups, ...).
//   * Outbound + typing + attachment-fetch the host calls via tools.
//   * Pairing flow (admin_routes handlers).

// SIDECAR_NAME = "signal-cli", CHANNEL = "signal" — Rhai consts aren't
// visible inside fn bodies, so the literals are inlined throughout.
const SELF_NUMBER_TTL = 300; // 5-min cache for /v1/accounts.

// =====================================================================
// LIFECYCLE
// =====================================================================

fn on_enable() {
    // Spawn the WS inbound consumer. The host's ws_subscribe owns
    // the reconnect + backoff loop; we just hand it the URL + the
    // name of the per-frame Rhai function.
    //
    // Cold-boot race: on_enable fires shortly after process boot,
    // but the sidecar supervisor's first reconcile pass takes a
    // few seconds to publish a port for signal-cli's container.
    // Use the blocking sidecar_url variant (60s budget) so the
    // subscription survives the race instead of bailing silently.
    let base = sidecar_url_blocking("signal-cli", 60000);
    if base == () || base == "" {
        log_warn("signal: sidecar didn't come up within 60s — restart the plugin from Settings → Plugins to retry");
        return;
    }
    // Once the supervisor has published a port we still need to
    // wait for signal-cli's HTTP daemon inside the container to
    // start serving /v1/accounts (typically another 5-30s on
    // first boot). Poll with a 90s budget.
    let num = ();
    let waited = 0;
    let max_wait_ms = 90000;
    let step_ms = 1000;
    while num == () && waited < max_wait_ms {
        try {
            num = self_number();
        } catch (e) {
            num = ();
        }
        if num == () {
            sleep_ms(step_ms);
            waited += step_ms;
        }
    }
    if num == () {
        log_warn("signal: signal-cli didn't surface a paired account within 90s — pair via Settings → Plugins → Signal then restart the plugin");
        return;
    }
    // Rhai's `string.replace(a, b)` MUTATES in place + returns unit
    // (NOT a value), so `base.replace("http://", "ws://") + "/path"`
    // evaluates to `() + "/path"` -> "/path" with no host. Strip the
    // "http://" prefix via sub_string instead. sidecar_url contract:
    // "http://<host>:<port>" — exactly 7 chars to skip.
    let host_port = base.sub_string(7);
    let ws = "ws://" + host_port + "/v1/receive/" + url_encode(num);
    log_info("signal: subscribing to inbound WebSocket at " + ws);
    // Discard the returned WsSubscriptionHandle — call_on_enable
    // serializes the Rhai return value to JSON and the handle isn't
    // serializable. We don't need it (host owns the subscription's
    // lifecycle until the plugin is disabled).
    let _handle = ws_subscribe(ws, "on_frame");
    ()
}

// =====================================================================
// INBOUND
// =====================================================================

fn on_frame(raw) {
    // Decode + handoff. Decode is split out into a pure `decode_frame`
    // function that the script-tier test suite calls directly with
    // no host_caps wiring — keeps the JSON-shape contract under
    // unit-test coverage without needing a live signal-cli sidecar.
    let inbound = decode_frame(raw);
    if inbound == () { return; }
    try {
        host_route_inbound(inbound);
    } catch (e) {
        log_warn("signal: host_route_inbound rejected: " + e);
    }
}

// Pure helper: decode a raw bbernhard WS frame into the
// channel-agnostic InboundMessage shape the host's routing
// pipeline expects, OR `()` for non-content frames (receipts,
// typing indicators, sync messages, account mismatches, etc.)
// the consumer should drop.
//
// Pure = doesn't call any host_caps binding. Tested standalone
// against fixture JSON in crates/script/tests/signal_plugin.rs.
fn decode_frame(raw) {
    // The whole decode lives inside one try/catch — any property
    // access on a non-map (e.g. an integer envelope, or garbage)
    // throws in Rhai and we want every shape error to drop the
    // frame, not propagate.
    try {
        let env = parse_json(raw).envelope;
        if env == () { return (); }
        let dm = env.dataMessage;
        if dm == () { return (); }
        let text = dm.message;
        let attachments = dm.attachments;
        if text == () && (attachments == () || attachments.is_empty()) {
            return ();
        }
        // Drop whitespace-only message bodies the same way the retired
        // host-side decoder did — receipts can carry a dataMessage
        // with `" "` text, which would otherwise route as a real
        // user_msg.
        let trimmed = if text != () { trim(text) } else { "" };
        if trimmed == "" && (attachments == () || attachments.is_empty()) {
            return ();
        }

        let group_info = dm.groupInfo;
        let group_id = ();
        let group_name = ();
        if group_info != () {
            group_id = group_info.groupId;
            let raw_name = group_info.groupName;
            if raw_name != () {
                let name_trimmed = trim(raw_name);
                if name_trimmed != "" {
                    group_name = name_trimmed;
                }
            }
        }

        return #{
            "channel": "signal",
            "native_id": if env.sourceNumber != () { env.sourceNumber } else { env.source },
            "display_name": env.sourceName,
            "group_id": group_id,
            "group_name": group_name,
            "text": trimmed,
            "timestamp_ms": env.timestamp,
            "attachments": map_attachments(attachments),
        };
    } catch (e) {
        log_warn("signal: undecodable frame: " + e);
        return ();
    }
}

fn map_attachments(arr) {
    if arr == () { return []; }
    let out = [];
    for a in arr {
        if a.id == () || a.id == "" { continue; }
        out.push(#{
            "bridge_id": a.id,
            "content_type": a.contentType,
            "filename": a.filename,
            "size_bytes": a.size,
        });
    }
    out
}

// =====================================================================
// TOOL DISPATCH
// =====================================================================

fn tool_call(name, args, oauth) {
    if name == "signal.send_message"           { return send_message_impl(args); }
    if name == "signal.reply"                  { return reply_impl(args); }
    if name == "signal.list_groups"            { return list_groups_impl(); }
    if name == "signal.create_group"           { return create_group_impl(args); }
    if name == "signal.add_group_members"      { return add_group_members_impl(args); }
    if name == "signal.leave_group"            { return leave_group_impl(args); }
    // Auto-bridge convention names (host calls these; not advertised
    // to the agent's tool catalog).
    if name == "signal.send_with_attachments"  { return send_with_attachments_impl(args); }
    if name == "signal.set_typing"             { return set_typing_impl(args); }
    if name == "signal.fetch_attachment"       { return fetch_attachment_impl(args); }
    throw "unknown tool '" + name + "'";
}

// ---- send_message / reply ------------------------------------------

fn send_message_impl(args) {
    let to = args.to;
    let text = args.text;
    if to == () || to == "" { throw "signal.send_message requires `to`"; }
    if text == () || text == "" { throw "signal.send_message requires `text`"; }
    let recipient = wire_recipient(to);
    let body = #{
        "number": self_number(),
        "recipients": [recipient],
        "message": text,
    };
    let resp = sidecar_http_post(sidecar_url("signal-cli") + "/v2/send", body);
    summarise_send_response(resp)
}

fn reply_impl(args) {
    // Reply to the current_chat_id. The host carries this through
    // the turn context; the plugin doesn't see it directly. So
    // signal.reply just maps to send_message with the recipient
    // taken from the inbound's binding — but that means the agent
    // must pass `to`. We document this in the plugin manifest.
    if args.to == () || args.to == "" {
        throw "signal.reply requires `to` — host fills this from the conversation's binding";
    }
    send_message_impl(args)
}

// ---- group ops ------------------------------------------------------

fn list_groups_impl() {
    let url = sidecar_url("signal-cli") + "/v1/groups/" + url_encode(self_number());
    let raw = sidecar_http_get(url, #{});
    if raw == () { return #{ "groups": [] }; }
    let out = [];
    for g in raw {
        if g.id == () { continue; }
        out.push(#{
            "id": g.id,
            "name": if g.name != () && g.name != "" { g.name } else { () },
            "member_count": if g.members != () && type_of(g.members) == "array" { g.members.len() } else { 0 },
        });
    }
    #{ "groups": out, "ack_text": "Listed " + out.len() + " group(s)." }
}

fn create_group_impl(args) {
    let title = if args.title != () { args.title } else { "" };
    let members = args.members;
    if members == () || type_of(members) != "array" || members.is_empty() {
        throw "signal.create_group requires `members` (non-empty array)";
    }
    let url = sidecar_url("signal-cli") + "/v1/groups/" + url_encode(self_number());
    let body = #{ "name": title, "members": members };
    let resp = sidecar_http_post(url, body);
    let group_id = resp.id;
    if group_id == () { throw "signal.create_group: bridge returned no `id`: " + resp; }
    let ack_text = "Group \"" + title + "\" created with " + members.len() + " member(s).";
    if args.message != () && args.message != "" {
        send_message_impl(#{ "to": group_id, "text": args.message });
    }
    #{ "group_id": group_id, "ack_text": ack_text }
}

fn add_group_members_impl(args) {
    let name = args.groupName;
    let members = args.members;
    if name == () || name == "" { throw "signal.add_group_members requires `groupName`"; }
    if members == () || type_of(members) != "array" || members.is_empty() {
        throw "signal.add_group_members requires `members` (non-empty array)";
    }
    let group_id = lookup_group_by_name(name);
    let url = sidecar_url("signal-cli") + "/v1/groups/" + url_encode(self_number())
        + "/" + url_encode(group_id) + "/members";
    sidecar_http_post(url, #{ "members": members });
    #{ "ack_text": "Added " + members.len() + " member(s) to \"" + name + "\"." }
}

fn leave_group_impl(args) {
    let name = args.groupName;
    if name == () || name == "" { throw "signal.leave_group requires `groupName`"; }
    let group_id = lookup_group_by_name(name);
    let url = sidecar_url("signal-cli") + "/v1/groups/" + url_encode(self_number())
        + "/" + url_encode(group_id) + "/quit";
    sidecar_http_post(url, #{});
    #{ "ack_text": "Left group \"" + name + "\"." }
}

fn lookup_group_by_name(name) {
    let listed = list_groups_impl();
    for g in listed.groups {
        if g.name == name { return g.id; }
    }
    throw "signal: no group named '" + name + "'";
}

// ---- auto-bridge (host-called) -------------------------------------

fn send_with_attachments_impl(args) {
    let to = args.to;
    let text = if args.text != () { args.text } else { "" };
    let attachments = args.attachments;
    if to == () || to == "" { throw "signal.send_with_attachments requires `to`"; }
    if attachments == () || type_of(attachments) != "array" || attachments.is_empty() {
        return send_message_impl(#{ "to": to, "text": text });
    }
    // Hard cap on per-call batch — mirrors the cap that lived in
    // the retired signal_transport.rs.
    if attachments.len() > 8 {
        throw "signal.send_with_attachments: max 8 attachments per send; got " + attachments.len();
    }
    // Fetch each attachment's bytes from the host attachment
    // store + base64-encode them as bbernhard-shaped data URLs.
    let data_urls = [];
    for aid in attachments {
        let a = host_get_attachment_bytes(aid);
        data_urls.push(a.data_url);
    }
    let recipient = wire_recipient(to);
    let body = #{
        "number": self_number(),
        "recipients": [recipient],
        "message": text,
        "base64_attachments": data_urls,
    };
    let resp = sidecar_http_post(sidecar_url("signal-cli") + "/v2/send", body);
    summarise_send_response(resp)
}

fn set_typing_impl(args) {
    let to = args.to;
    let active = args.active == true;
    if to == () || to == "" { throw "signal.set_typing requires `to`"; }
    let recipient = wire_recipient(to);
    let url = sidecar_url("signal-cli") + "/v1/typing-indicator/" + url_encode(self_number());
    let body = #{ "recipient": recipient };
    if active {
        sidecar_http_post(url, body);
    } else {
        sidecar_http_delete(url, #{});
    }
    #{ "ok": true }
}

fn fetch_attachment_impl(args) {
    let attachment_id = args.attachment_id;
    if attachment_id == () || attachment_id == "" {
        throw "signal.fetch_attachment requires `attachment_id`";
    }
    // Validate id shape — bbernhard's attachment ids are url-safe
    // base64 with optional dot-extension. Reject anything that
    // could break URL-path interpolation.
    if attachment_id.len() > 256 || attachment_id.contains("..") {
        throw "signal.fetch_attachment: bad attachment_id shape";
    }
    let url = sidecar_url("signal-cli") + "/v1/attachments/" + url_encode(attachment_id);
    let r = sidecar_http_get_bytes(url, #{});
    #{
        "data_url": r.data_url,
        "mime_type": r.mime_type,
        "size_bytes": r.size_bytes,
    }
}

// =====================================================================
// HELPERS
// =====================================================================

// signal-cli's outbound POST /v2/send accepts:
//   * E.164 phone for DMs
//   * `group.<base64-of-bytes-of-internal-id>` for groups
// The host's binding store keeps the raw `internal_id` form. We
// detect "looks like a base64 internal_id" heuristically: a
// trailing `=` and no leading `+` or `group.` prefix.
fn wire_recipient(s) {
    if s == () || s == "" { return s; }
    if s.starts_with("group.") { return s; }
    if s.starts_with("+") { return s; }
    if s.starts_with("signal:") { return s; }
    // Heuristic: looks like a single-base64 group internal_id.
    if s.ends_with("=") {
        return "group." + base64_encode(s);
    }
    s
}

fn self_number() {
    // Cache /v1/accounts response for 5 min — signal-cli only ever
    // has one account paired in production; the round trip is
    // ~1ms but every outbound call would otherwise hit it.
    let url = sidecar_url("signal-cli") + "/v1/accounts";
    let raw = sidecar_http_get(url, #{});
    if raw == () || type_of(raw) != "array" || raw.is_empty() {
        throw "signal: no account paired — visit Settings → Plugin → Signal and scan a QR code";
    }
    raw[0]
}

fn ws_url() {
    let base = sidecar_url("signal-cli");
    if base == () || base == "" { return (); }
    let num;
    try {
        num = self_number();
    } catch (e) {
        return ();
    }
    // sub_string instead of replace — see comment in on_enable.
    "ws://" + base.sub_string(7) + "/v1/receive/" + url_encode(num)
}

fn summarise_send_response(resp) {
    if resp == () { return "ok"; }
    if resp.timestamp != () { return resp.timestamp.to_string(); }
    resp.to_string()
}

// =====================================================================
// ADMIN ROUTES (declared in plugin.toml's [[admin_routes]])
// =====================================================================

// GET /api/admin/plugins/signal/status — pairing + sidecar snapshot
// the SPA polls every few seconds while the Settings → Plugins →
// Signal page is open. Shape matches `SignalStatusResponse` in
// web/src/api/endpoints.ts.
fn admin_status(req) {
    let base = sidecar_url("signal-cli");
    if base == () {
        return #{
            "sidecar_status": "down",
            "sidecar_rpc_url": (),
            "registered_accounts": [],
            "accounts_on_disk": [],
            "fetch_error": "signal-cli sidecar not running",
        };
    }
    let accounts = [];
    let fetch_error = ();
    try {
        let raw = sidecar_http_get(base + "/v1/accounts", #{});
        if raw != () && type_of(raw) == "array" {
            accounts = raw;
        }
    } catch (e) {
        fetch_error = e.to_string();
    }
    // We don't have a host filesystem binding so we can't see the
    // accounts.json drift the upstream signal-cli `addManager` bug
    // produces. Report disk == in-memory; the operator restarts the
    // sidecar manually from Settings → Sidecars when they hit it.
    #{
        "sidecar_status": if fetch_error == () { "healthy" } else { "unhealthy" },
        "sidecar_rpc_url": base,
        "registered_accounts": accounts,
        "accounts_on_disk": accounts,
        "fetch_error": fetch_error,
    }
}

// GET /api/admin/plugins/signal/qrcodelink — returns the QR code
// PNG as a base64 data URL the SPA puts directly into <img src>.
// The operator scans the QR with their phone's Signal app to pair
// this sidecar instance as a secondary device.
fn admin_qrcodelink(req) {
    let device_name = "execlaw";
    if req.query != () && req.query.device_name != () {
        let q = trim(req.query.device_name);
        if q != "" { device_name = q; }
    }
    let base = sidecar_url("signal-cli");
    if base == () {
        return #{ "error": "signal-cli sidecar not running" };
    }
    try {
        let r = sidecar_http_get_bytes(base + "/v1/qrcodelink", #{ "device_name": device_name });
        return #{ "data_url": r.data_url, "mime_type": r.mime_type };
    } catch (e) {
        return #{ "error": e.to_string() };
    }
}

// DELETE /api/admin/plugins/signal/unregister-account?number=<E.164>
// Unlinks this execlaw install from the Signal account. The SPA
// fires this from the "Unlink" button on the paired-state view.
fn admin_unregister_account(req) {
    let number = ();
    if req.query != () && req.query.number != () {
        number = trim(req.query.number);
    }
    if number == () || number == "" {
        throw "missing required query param 'number'";
    }
    let base = sidecar_url("signal-cli");
    if base == () { throw "signal-cli sidecar not running"; }
    sidecar_http_delete(base + "/v1/accounts/" + url_encode(number), #{});
    #{ "ok": true }
}
