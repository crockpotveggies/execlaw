// whatsapp — WhatsApp Multi-Device transport via wuzapi.
//
// Same shape as the Signal plugin (post-v0.4.7): the host owns
// trust admit + conversation routing + agent reply auto-bridge;
// this plugin owns the inbound WS consumer, frame decoder, all
// `whatsapp.*` tools, and the QR pairing flow.
//
// wuzapi quirks vs bbernhard:
//   * Multi-tenant: every HTTP call needs a `Token: <user_token>`
//     header. The token is provisioned via /admin/users at
//     on_enable (admin token comes from the manifest's services.env)
//     and then cached in the per-plugin vault.
//   * WS endpoint: ws://host:port/ws?token=<user_token>.
//   * Recipient JID format: `<digits>@s.whatsapp.net` for 1:1,
//     `<id>@g.us` for groups. wire_recipient() massages plain
//     phone numbers into the @s.whatsapp.net form.

// =====================================================================
// LIFECYCLE
// =====================================================================

fn on_enable() {
    // Wait for the wuzapi container to publish a port.
    let base = sidecar_url_blocking("wuzapi", 60000);
    if base == () || base == "" {
        log_warn("whatsapp: sidecar didn't come up within 60s — restart the plugin from Settings → Plugins to retry");
        return;
    }

    // Provision (or look up) the per-execlaw user, cache its
    // token in the vault. The token doesn't change across reboots
    // unless wuzapi's database is wiped, so vault hits 99% of the
    // time after first install.
    let user_token = ();
    try {
        user_token = ensure_user_token(base);
    } catch (e) {
        log_warn("whatsapp: couldn't provision wuzapi user: " + e + " — pair via Settings → Plugins → WhatsApp once the sidecar is healthy");
        return;
    }
    if user_token == () || user_token == "" {
        log_warn("whatsapp: ensure_user_token returned empty; bailing");
        return;
    }

    // Subscribe to the per-user WS event stream. wuzapi accepts
    // the token via the `?token=` query param OR the Token: header
    // — we use the query param since ws_subscribe doesn't support
    // headers yet.
    let host_port = base.sub_string(7); // strip "http://"
    let ws = "ws://" + host_port + "/ws?token=" + url_encode(user_token);
    log_info("whatsapp: subscribing to inbound WebSocket at ws://" + host_port + "/ws");
    let _handle = ws_subscribe(ws, "on_frame");
    ()
}

// Provision-or-lookup the "execlaw" wuzapi user. The endpoint
// shape is wuzapi-specific: POST /admin/users with the admin
// token returns either the new user OR an "already exists" hint.
// We then GET /admin/users to find by name + extract the token.
fn ensure_user_token(base) {
    // Cache hit?
    let cached = vault_get("user_token");
    if cached != () && cached != "" { return cached; }

    let admin = #{ "Authorization": "execlaw-wuzapi-admin", "Token": "execlaw-wuzapi-admin" };

    // Try to create the user. Idempotent on wuzapi >= 1.x — a
    // duplicate name returns 409 / 400 with an "already exists"
    // body, which we treat as success and fall through to lookup.
    try {
        sidecar_http_post(base + "/admin/users", #{
            "name": "execlaw",
            "token": "",
        }, admin);
    } catch (e) {
        // Best-effort: log, then try to look up by name anyway.
        log_info("whatsapp: /admin/users create returned: " + e);
    }

    // List + find. wuzapi's /admin/users response shape varies
    // by version — handle both `[{...}]` and `{users:[{...}]}`.
    let raw = sidecar_http_get(base + "/admin/users", #{}, admin);
    let users = raw;
    if type_of(raw) == "map" && raw.users != () {
        users = raw.users;
    }
    if users == () || type_of(users) != "array" {
        throw "wuzapi /admin/users returned unexpected shape: " + raw;
    }
    for u in users {
        let name = if u.name != () { u.name } else { u.Name };
        if name == "execlaw" {
            let token = if u.token != () { u.token } else { u.Token };
            if token != () && token != "" {
                vault_put("user_token", token);
                return token;
            }
        }
    }
    throw "wuzapi: no user named 'execlaw' after create + list";
}

// =====================================================================
// INBOUND
// =====================================================================

fn on_frame(raw) {
    let inbound = decode_frame(raw);
    if inbound == () { return; }
    try {
        host_route_inbound(inbound);
    } catch (e) {
        log_warn("whatsapp: host_route_inbound rejected: " + e);
    }
}

// Pure helper: decode a wuzapi WS event into the channel-agnostic
// InboundMessage shape OR `()` for non-content events (status
// updates, presence, receipts) the consumer should drop.
//
// wuzapi event envelope (best-effort against the documented API
// — the actual shape may vary by version + needs iteration when
// the operator first pairs):
//   {
//     "type": "Message",
//     "event": {
//       "Info": {
//         "Chat": "<jid>@s.whatsapp.net" | "<id>@g.us",
//         "Sender": "<jid>@s.whatsapp.net",
//         "PushName": "Justin",
//         "Timestamp": 1234567890,
//         "IsGroup": false
//       },
//       "Message": {
//         "conversation": "Hello"
//       }
//     }
//   }
fn decode_frame(raw) {
    try {
        let env = parse_json(raw);
        if env == () { return (); }
        if env.type != () && env.type != "Message" { return (); }

        let event = env.event;
        if event == () { return (); }
        let info = event.Info;
        let msg = event.Message;
        if info == () || msg == () { return (); }

        // Pull the text out of the various message variants. The
        // most common is `conversation` for plain text; richer
        // types (extendedTextMessage, imageMessage with caption,
        // etc.) live under nested keys.
        let text = ();
        if msg.conversation != () { text = msg.conversation; }
        if text == () && msg.extendedTextMessage != () && msg.extendedTextMessage.text != () {
            text = msg.extendedTextMessage.text;
        }
        if text == () && msg.imageMessage != () && msg.imageMessage.caption != () {
            text = msg.imageMessage.caption;
        }
        if text == () && msg.documentMessage != () && msg.documentMessage.caption != () {
            text = msg.documentMessage.caption;
        }
        let trimmed = if text != () { trim(text) } else { "" };

        // Attachments: image / video / audio / document. Each
        // returns a media_id we'd POST back to wuzapi to
        // download. Capture the media_id as bridge_id; the host
        // will call whatsapp.fetch_attachment with it later.
        let attachments = [];
        for kind in ["imageMessage", "videoMessage", "audioMessage", "documentMessage"] {
            let m = msg[kind];
            if m == () { continue; }
            let bridge_id = if m.url != () { m.url } else if m.directPath != () { m.directPath } else { () };
            if bridge_id == () { continue; }
            attachments.push(#{
                "bridge_id": bridge_id,
                "content_type": m.mimetype,
                "filename": if m.fileName != () { m.fileName } else { () },
                "size_bytes": m.fileLength,
            });
        }

        if trimmed == "" && attachments.is_empty() { return (); }

        let chat = info.Chat;
        let sender = info.Sender;
        let is_group = info.IsGroup == true || (chat != () && chat.contains("@g.us"));

        // Foreign id: for 1:1, the sender's JID (or chat, same thing).
        // For groups, the sender's JID — the chat is the group's JID
        // and goes into group_id.
        let native_id = if sender != () { strip_jid(sender) } else { strip_jid(chat) };
        let group_id = if is_group { chat } else { () };

        return #{
            "channel": "whatsapp",
            "native_id": native_id,
            "display_name": info.PushName,
            "group_id": group_id,
            "group_name": (), // wuzapi doesn't include group name on every event
            "text": trimmed,
            "timestamp_ms": if info.Timestamp != () { info.Timestamp * 1000 } else { () },
            "attachments": attachments,
        };
    } catch (e) {
        log_warn("whatsapp: undecodable frame: " + e);
        return ();
    }
}

// Strip `@s.whatsapp.net` / `@g.us` suffix to recover the bare
// digits or id. Used for the InboundMessage.native_id field —
// the host's principal admit pipeline keys on plain phone, not
// wuzapi-formatted JID.
fn strip_jid(s) {
    if s == () || s == "" { return s; }
    let at = s.index_of("@");
    if at < 0 { return s; }
    s.sub_string(0, at)
}

// =====================================================================
// TOOL DISPATCH
// =====================================================================

fn tool_call(name, args, oauth) {
    if name == "whatsapp.send_message"           { return send_message_impl(args); }
    if name == "whatsapp.reply"                  { return reply_impl(args); }
    if name == "whatsapp.list_groups"            { return list_groups_impl(); }
    if name == "whatsapp.create_group"           { return create_group_impl(args); }
    if name == "whatsapp.add_group_members"      { return add_group_members_impl(args); }
    if name == "whatsapp.leave_group"            { return leave_group_impl(args); }
    if name == "whatsapp.send_with_attachments"  { return send_with_attachments_impl(args); }
    if name == "whatsapp.set_typing"             { return set_typing_impl(args); }
    if name == "whatsapp.fetch_attachment"       { return fetch_attachment_impl(args); }
    throw "unknown tool '" + name + "'";
}

// ---- send_message / reply ------------------------------------------

fn send_message_impl(args) {
    let to = args.to;
    let text = args.text;
    if to == () || to == "" { throw "whatsapp.send_message requires `to`"; }
    if text == () || text == "" { throw "whatsapp.send_message requires `text`"; }
    let phone_jid = wire_recipient(to);
    let body = #{
        "Phone": phone_jid,
        "Body": text,
    };
    let resp = sidecar_http_post(sidecar_url("wuzapi") + "/chat/send/text", body, user_headers());
    summarise_send_response(resp)
}

fn reply_impl(args) {
    if args.to == () || args.to == "" {
        throw "whatsapp.reply requires `to` — host fills this from the conversation's binding";
    }
    send_message_impl(args)
}

// ---- group ops ------------------------------------------------------

fn list_groups_impl() {
    let url = sidecar_url("wuzapi") + "/group/list";
    let raw = sidecar_http_get(url, #{}, user_headers());
    if raw == () { return #{ "groups": [] }; }
    // wuzapi returns either {data: {groups: [...]}} or [...] — handle both.
    let groups = raw;
    if type_of(raw) == "map" {
        if raw.data != () && raw.data.groups != () {
            groups = raw.data.groups;
        } else if raw.groups != () {
            groups = raw.groups;
        }
    }
    if type_of(groups) != "array" { return #{ "groups": [] }; }
    let out = [];
    for g in groups {
        let id = if g.JID != () { g.JID } else if g.id != () { g.id } else { () };
        if id == () { continue; }
        out.push(#{
            "id": id,
            "name": if g.Name != () { g.Name } else if g.name != () { g.name } else { () },
            "member_count": if g.Participants != () && type_of(g.Participants) == "array" { g.Participants.len() } else { 0 },
        });
    }
    #{ "groups": out, "ack_text": "Listed " + out.len() + " group(s)." }
}

fn create_group_impl(args) {
    let title = if args.title != () { args.title } else { "" };
    let members = args.members;
    if title == "" { throw "whatsapp.create_group requires `title`"; }
    if members == () || type_of(members) != "array" || members.is_empty() {
        throw "whatsapp.create_group requires `members` (non-empty array)";
    }
    let participants = [];
    for m in members {
        participants.push(wire_recipient(m));
    }
    let url = sidecar_url("wuzapi") + "/group/create";
    let body = #{ "Name": title, "Participants": participants };
    let resp = sidecar_http_post(url, body, user_headers());
    let group_id = if resp.JID != () { resp.JID } else if resp.id != () { resp.id } else if resp.data != () && resp.data.JID != () { resp.data.JID } else { () };
    if group_id == () { throw "whatsapp.create_group: bridge returned no `JID`: " + resp; }
    let ack_text = "Group \"" + title + "\" created with " + members.len() + " member(s).";
    if args.message != () && args.message != "" {
        send_message_impl(#{ "to": group_id, "text": args.message });
    }
    #{ "group_id": group_id, "ack_text": ack_text }
}

fn add_group_members_impl(args) {
    let name = args.groupName;
    let members = args.members;
    if name == () || name == "" { throw "whatsapp.add_group_members requires `groupName`"; }
    if members == () || type_of(members) != "array" || members.is_empty() {
        throw "whatsapp.add_group_members requires `members` (non-empty array)";
    }
    let group_id = lookup_group_by_name(name);
    let participants = [];
    for m in members {
        participants.push(wire_recipient(m));
    }
    let url = sidecar_url("wuzapi") + "/group/updateparticipants";
    sidecar_http_post(url, #{
        "GroupJID": group_id,
        "Phone": participants,
        "Action": "add",
    }, user_headers());
    #{ "ack_text": "Added " + members.len() + " member(s) to \"" + name + "\"." }
}

fn leave_group_impl(args) {
    let name = args.groupName;
    if name == () || name == "" { throw "whatsapp.leave_group requires `groupName`"; }
    let group_id = lookup_group_by_name(name);
    let url = sidecar_url("wuzapi") + "/group/leave";
    sidecar_http_post(url, #{ "GroupJID": group_id }, user_headers());
    #{ "ack_text": "Left group \"" + name + "\"." }
}

fn lookup_group_by_name(name) {
    let listed = list_groups_impl();
    for g in listed.groups {
        if g.name == name { return g.id; }
    }
    throw "whatsapp: no group named '" + name + "'";
}

// ---- auto-bridge (host-called) -------------------------------------

fn send_with_attachments_impl(args) {
    let to = args.to;
    let text = if args.text != () { args.text } else { "" };
    let attachments = args.attachments;
    if to == () || to == "" { throw "whatsapp.send_with_attachments requires `to`"; }
    if attachments == () || type_of(attachments) != "array" || attachments.is_empty() {
        return send_message_impl(#{ "to": to, "text": text });
    }
    if attachments.len() > 8 {
        throw "whatsapp.send_with_attachments: max 8 attachments per send; got " + attachments.len();
    }
    let phone_jid = wire_recipient(to);
    // wuzapi takes one media at a time on /chat/send/{image,document,etc}.
    // For multi-attachment batches, send each in series; the text
    // rides on the first.
    let last_resp = ();
    let i = 0;
    for aid in attachments {
        let a = host_get_attachment_bytes(aid);
        // Pick the right wuzapi endpoint by mime type.
        let path = "/chat/send/document";
        if a.mime_type != () && a.mime_type.starts_with("image/") {
            path = "/chat/send/image";
        } else if a.mime_type != () && a.mime_type.starts_with("video/") {
            path = "/chat/send/video";
        } else if a.mime_type != () && a.mime_type.starts_with("audio/") {
            path = "/chat/send/audio";
        }
        let body = #{
            "Phone": phone_jid,
            "Image": a.data_url, // wuzapi accepts base64 data URLs in the media field
            "Document": a.data_url,
            "Video": a.data_url,
            "Audio": a.data_url,
            "Caption": if i == 0 { text } else { "" },
        };
        last_resp = sidecar_http_post(sidecar_url("wuzapi") + path, body, user_headers());
        i += 1;
    }
    summarise_send_response(last_resp)
}

fn set_typing_impl(args) {
    let to = args.to;
    let active = args.active == true;
    if to == () || to == "" { throw "whatsapp.set_typing requires `to`"; }
    let phone_jid = wire_recipient(to);
    let url = sidecar_url("wuzapi") + "/chat/presence";
    let body = #{
        "Phone": phone_jid,
        "State": if active { "composing" } else { "paused" },
    };
    sidecar_http_post(url, body, user_headers());
    #{ "ok": true }
}

fn fetch_attachment_impl(args) {
    let attachment_id = args.attachment_id;
    if attachment_id == () || attachment_id == "" {
        throw "whatsapp.fetch_attachment requires `attachment_id`";
    }
    // wuzapi exposes /chat/downloadimage etc. Multiple endpoints
    // by media kind, but the simplest is to use the generic
    // download with the media URL/path.
    let url = sidecar_url("wuzapi") + "/chat/downloadimage";
    let r = sidecar_http_get_bytes(url, #{ "url": attachment_id }, user_headers());
    #{
        "data_url": r.data_url,
        "mime_type": r.mime_type,
        "size_bytes": r.size_bytes,
    }
}

// =====================================================================
// HELPERS
// =====================================================================

// Convert a plain phone (E.164 or digits-only) into wuzapi's JID
// shape. Group ids (`<id>@g.us`) and already-formed JIDs pass
// through unchanged.
fn wire_recipient(s) {
    if s == () || s == "" { return s; }
    if s.contains("@") { return s; } // already a JID
    let digits = s;
    if digits.starts_with("+") {
        digits = digits.sub_string(1);
    }
    digits + "@s.whatsapp.net"
}

// Build the per-request headers map. wuzapi accepts `Token: <user_token>`;
// we send it on every call so the sidecar associates the request
// with our provisioned user.
fn user_headers() {
    let token = vault_get("user_token");
    if token == () || token == "" {
        throw "whatsapp: no user_token in vault — on_enable hasn't completed yet, or the wuzapi sidecar is down";
    }
    #{ "Token": token, "Authorization": token }
}

fn summarise_send_response(resp) {
    if resp == () { return "ok"; }
    if resp.Details != () { return resp.Details; }
    if resp.id != () { return resp.id; }
    "ok"
}

// =====================================================================
// ADMIN ROUTES (declared in plugin.toml's [[admin_routes]])
// =====================================================================

// GET /api/admin/plugins/whatsapp/status — pairing + sidecar
// snapshot the SPA polls. Same shape as Signal's admin_status so
// the SPA's WhatsApp page can reuse the SignalStatusResponse
// shape (sidecar_status + registered_accounts + fetch_error).
fn admin_status(req) {
    let base = sidecar_url("wuzapi");
    if base == () {
        return #{
            "sidecar_status": "down",
            "sidecar_rpc_url": (),
            "registered_accounts": [],
            "accounts_on_disk": [],
            "fetch_error": "wuzapi sidecar not running",
        };
    }
    let accounts = [];
    let fetch_error = ();
    try {
        // /session/info reflects the per-user session state.
        let info = sidecar_http_get(base + "/session/info", #{}, user_headers());
        if info != () {
            // wuzapi's connected/loggedin booleans tell us if a phone
            // is paired. Surface the connected JID as a one-element
            // accounts list so the SPA's "registered_accounts" badge
            // works the same way as Signal.
            let logged = info.LoggedIn == true || info.loggedIn == true ||
                (info.data != () && info.data.LoggedIn == true);
            if logged {
                let jid = ();
                if info.JID != () { jid = info.JID; }
                else if info.data != () && info.data.JID != () { jid = info.data.JID; }
                if jid != () && jid != "" {
                    accounts.push(strip_jid(jid));
                }
            }
        }
    } catch (e) {
        fetch_error = e.to_string();
    }
    #{
        "sidecar_status": if fetch_error == () { "healthy" } else { "unhealthy" },
        "sidecar_rpc_url": base,
        "registered_accounts": accounts,
        "accounts_on_disk": accounts,
        "fetch_error": fetch_error,
    }
}

// GET /api/admin/plugins/whatsapp/qrcodelink — returns the QR
// code data URL the SPA <img>-renders. Operator scans it from
// WhatsApp on their phone (Settings → Linked devices → Link a
// device) to pair this wuzapi instance.
fn admin_qrcodelink(req) {
    let base = sidecar_url("wuzapi");
    if base == () {
        return #{ "error": "wuzapi sidecar not running" };
    }
    try {
        let r = sidecar_http_get_bytes(base + "/session/qr", #{}, user_headers());
        return #{ "data_url": r.data_url, "mime_type": r.mime_type };
    } catch (e) {
        return #{ "error": e.to_string() };
    }
}

// DELETE /api/admin/plugins/whatsapp/unregister-account — logs
// the wuzapi user out of WhatsApp + clears the cached user_token.
// Forces a fresh QR scan on next pairing.
fn admin_unregister_account(req) {
    let base = sidecar_url("wuzapi");
    if base == () { throw "wuzapi sidecar not running"; }
    try {
        sidecar_http_post(base + "/session/logout", #{}, user_headers());
    } catch (e) {
        log_warn("whatsapp: /session/logout returned: " + e);
    }
    vault_delete("user_token");
    #{ "ok": true }
}
