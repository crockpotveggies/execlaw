// whatsapp — WhatsApp Multi-Device transport via wuzapi.
//
// Same shape as the Signal plugin (post-v0.4.7). The host owns
// trust admit + conversation routing + agent reply auto-bridge;
// this plugin owns the inbound WS consumer, frame decoder, all
// `whatsapp.*` tools, and the QR pairing flow.
//
// wuzapi contract (probed live against asternic/wuzapi:latest,
// 2026-05-08):
//
//   * Health probe: `GET /` returns 200 (no /health, no /api/version).
//   * Admin auth: `Authorization: <admin_token>` header. Admin
//     token comes from the manifest's WUZAPI_ADMIN_TOKEN env var.
//   * User auth: `Token: <user_token>` header. The user token
//     is set explicitly when we POST /admin/users so we know it
//     ahead of time without parsing the response.
//   * POST /admin/users  body  {name, token, webhook, events}
//                        resp  {code, data: {...full user...}, success}
//                        409 / 500 on duplicate name (which is fine —
//                        we treat it as "already provisioned" and
//                        continue with the known token).
//   * GET  /admin/users  resp  {code, data: [...], success}
//   * POST /session/connect  body  {Subscribe: ["Message"], Immediate: true}
//                            MUST be called once before /session/qr
//                            stops returning "no session".
//   * GET  /session/status  resp  {data: {connected, loggedIn, jid, qrcode, ...}}
//   * GET  /session/qr      resp  {data: {QRCode: "data:image/png;base64,..."}}
//                            (NOT raw PNG bytes — JSON-wrapped data URL)
//   * POST /chat/send/text  body  {Phone, Body}
//   * Group ops: /group/list, /group/create, /group/updateparticipants,
//     /group/leave  — same Token header, varies by version.
//
// Deferred: end-to-end test of group + attachment paths against
// a real paired account. The endpoints + body shapes match
// wuzapi's documented API but field-casing drift is possible.

const USER_TOKEN_NAME = "execlaw-user";

// =====================================================================
// LIFECYCLE
// =====================================================================

fn on_enable() {
    let base = sidecar_url_blocking("wuzapi", 60000);
    if base == () || base == "" {
        log_warn("whatsapp: sidecar didn't come up within 60s — restart the plugin from Settings → Plugins to retry");
        return;
    }
    // Provision (or look up) the per-execlaw user. The token is
    // pinned ("execlaw-user") so we don't have to round-trip the
    // /admin/users response. Vault caches the same value to keep
    // user_headers() honest if wuzapi's env / db gets reset.
    try {
        ensure_user_token(base);
    } catch (e) {
        log_warn("whatsapp: ensure_user_token failed: " + e + " — Settings → Plugins → WhatsApp will surface the error and the operator can retry once wuzapi is healthy");
        return;
    }
    // Connect the session so /session/qr starts returning the
    // QR code instead of 500-ing with "no session". Idempotent —
    // wuzapi treats a second /session/connect on a connected
    // session as a no-op.
    try {
        ensure_session_connected(base);
    } catch (e) {
        log_warn("whatsapp: /session/connect failed: " + e + " — pairing UI will retry on each poll");
    }
    let host_port = base.sub_string(7); // strip "http://"
    let ws = "ws://" + host_port + "/ws?token=" + url_encode("execlaw-user");
    log_info("whatsapp: subscribing to inbound WebSocket at ws://" + host_port + "/ws");
    let _handle = ws_subscribe(ws, "on_frame");
    ()
}

// Idempotent provision of the "execlaw" user. Token is pinned so
// we always know what value to send in user_headers(). Already-
// existing user surfaces as a 4xx/500 from POST; we ignore that
// and verify via GET /admin/users.
fn ensure_user_token(base) {
    // Cache hit fast-path.
    let cached = vault_get("user_token");
    if cached != () && cached != "" { return cached; }

    let admin_h = #{ "Authorization": "execlaw-wuzapi-admin" };
    let token = "execlaw-user";

    // Best-effort create. Treat any failure as "user probably
    // already exists" and fall through to the listing check.
    try {
        sidecar_http_post(base + "/admin/users", #{
            "name": "execlaw",
            "token": token,
            "webhook": "",
            "events": "All",
        }, admin_h);
        log_info("whatsapp: provisioned wuzapi user 'execlaw'");
    } catch (e) {
        log_info("whatsapp: /admin/users create returned " + e + " (proceeding to verify)");
    }

    // Verify by listing. wuzapi wraps the array in `data`.
    let raw = sidecar_http_get(base + "/admin/users", #{}, admin_h);
    let users = ();
    if type_of(raw) == "map" && raw.data != () {
        users = raw.data;
    } else if type_of(raw) == "array" {
        users = raw;
    }
    if users == () || type_of(users) != "array" {
        throw "wuzapi /admin/users returned unexpected shape";
    }
    let found = false;
    for u in users {
        let name = if u.name != () { u.name } else { u.Name };
        if name == "execlaw" { found = true; break; }
    }
    if !found {
        throw "wuzapi: 'execlaw' user not present after create + list";
    }
    vault_put("user_token", token);
    token
}

// POST /session/connect. Idempotent — wuzapi reports "Connected"
// on already-connected sessions. Required before /session/qr
// will return a QR; otherwise that endpoint 500s with "no session".
fn ensure_session_connected(base) {
    sidecar_http_post(base + "/session/connect", #{
        "Subscribe": ["Message"],
        "Immediate": true,
    }, user_headers());
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
// InboundMessage shape OR `()` for non-content events.
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

        let attachments = [];
        if msg.imageMessage != () {
            let m = msg.imageMessage;
            let bridge_id = if m.url != () { m.url } else if m.directPath != () { m.directPath } else { () };
            if bridge_id != () {
                attachments.push(#{ "bridge_id": bridge_id, "content_type": m.mimetype, "filename": m.fileName, "size_bytes": m.fileLength });
            }
        }
        if msg.documentMessage != () {
            let m = msg.documentMessage;
            let bridge_id = if m.url != () { m.url } else if m.directPath != () { m.directPath } else { () };
            if bridge_id != () {
                attachments.push(#{ "bridge_id": bridge_id, "content_type": m.mimetype, "filename": m.fileName, "size_bytes": m.fileLength });
            }
        }
        if msg.videoMessage != () {
            let m = msg.videoMessage;
            let bridge_id = if m.url != () { m.url } else if m.directPath != () { m.directPath } else { () };
            if bridge_id != () {
                attachments.push(#{ "bridge_id": bridge_id, "content_type": m.mimetype, "filename": m.fileName, "size_bytes": m.fileLength });
            }
        }
        if msg.audioMessage != () {
            let m = msg.audioMessage;
            let bridge_id = if m.url != () { m.url } else if m.directPath != () { m.directPath } else { () };
            if bridge_id != () {
                attachments.push(#{ "bridge_id": bridge_id, "content_type": m.mimetype, "filename": (), "size_bytes": m.fileLength });
            }
        }

        if trimmed == "" && attachments.is_empty() { return (); }

        let chat = info.Chat;
        let sender = info.Sender;
        let is_group = info.IsGroup == true || (chat != () && chat.contains("@g.us"));

        let native_id = if sender != () { strip_jid(sender) } else { strip_jid(chat) };
        let group_id = if is_group { chat } else { () };

        return #{
            "channel": "whatsapp",
            "native_id": native_id,
            "display_name": info.PushName,
            "group_id": group_id,
            "group_name": (),
            "text": trimmed,
            "timestamp_ms": if info.Timestamp != () { info.Timestamp * 1000 } else { () },
            "attachments": attachments,
        };
    } catch (e) {
        log_warn("whatsapp: undecodable frame: " + e);
        return ();
    }
}

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

fn send_message_impl(args) {
    let to = args.to;
    let text = args.text;
    if to == () || to == "" { throw "whatsapp.send_message requires `to`"; }
    if text == () || text == "" { throw "whatsapp.send_message requires `text`"; }
    let resp = sidecar_http_post(sidecar_url("wuzapi") + "/chat/send/text", #{
        "Phone": wire_recipient(to),
        "Body": text,
    }, user_headers());
    summarise_send_response(resp)
}

fn reply_impl(args) {
    if args.to == () || args.to == "" {
        throw "whatsapp.reply requires `to` — host fills this from the conversation's binding";
    }
    send_message_impl(args)
}

fn list_groups_impl() {
    let raw = sidecar_http_get(sidecar_url("wuzapi") + "/group/list", #{}, user_headers());
    let groups = ();
    if type_of(raw) == "map" {
        if raw.data != () {
            // wuzapi often nests further: {data: {Groups: [...]}}.
            if type_of(raw.data) == "array" { groups = raw.data; }
            else if raw.data.Groups != () { groups = raw.data.Groups; }
            else if raw.data.groups != () { groups = raw.data.groups; }
        }
    } else if type_of(raw) == "array" {
        groups = raw;
    }
    if groups == () || type_of(groups) != "array" { return #{ "groups": [] }; }
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
    for m in members { participants.push(wire_recipient(m)); }
    let resp = sidecar_http_post(sidecar_url("wuzapi") + "/group/create", #{
        "Name": title,
        "Participants": participants,
    }, user_headers());
    let group_id = ();
    if type_of(resp) == "map" {
        if resp.data != () && resp.data.JID != () { group_id = resp.data.JID; }
        else if resp.JID != () { group_id = resp.JID; }
    }
    if group_id == () { throw "whatsapp.create_group: bridge returned no JID"; }
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
    for m in members { participants.push(wire_recipient(m)); }
    sidecar_http_post(sidecar_url("wuzapi") + "/group/updateparticipants", #{
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
    sidecar_http_post(sidecar_url("wuzapi") + "/group/leave", #{
        "GroupJID": group_id,
    }, user_headers());
    #{ "ack_text": "Left group \"" + name + "\"." }
}

fn lookup_group_by_name(name) {
    let listed = list_groups_impl();
    for g in listed.groups {
        if g.name == name { return g.id; }
    }
    throw "whatsapp: no group named '" + name + "'";
}

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
    let last_resp = ();
    let i = 0;
    for aid in attachments {
        let a = host_get_attachment_bytes(aid);
        let path = "/chat/send/document";
        let media_field = "Document";
        if a.mime_type != () && a.mime_type.starts_with("image/") {
            path = "/chat/send/image";
            media_field = "Image";
        } else if a.mime_type != () && a.mime_type.starts_with("video/") {
            path = "/chat/send/video";
            media_field = "Video";
        } else if a.mime_type != () && a.mime_type.starts_with("audio/") {
            path = "/chat/send/audio";
            media_field = "Audio";
        }
        let body = #{
            "Phone": phone_jid,
            "Caption": if i == 0 { text } else { "" },
        };
        body[media_field] = a.data_url;
        last_resp = sidecar_http_post(sidecar_url("wuzapi") + path, body, user_headers());
        i += 1;
    }
    summarise_send_response(last_resp)
}

fn set_typing_impl(args) {
    let to = args.to;
    let active = args.active == true;
    if to == () || to == "" { throw "whatsapp.set_typing requires `to`"; }
    sidecar_http_post(sidecar_url("wuzapi") + "/chat/presence", #{
        "Phone": wire_recipient(to),
        "State": if active { "composing" } else { "paused" },
    }, user_headers());
    #{ "ok": true }
}

fn fetch_attachment_impl(args) {
    let attachment_id = args.attachment_id;
    if attachment_id == () || attachment_id == "" {
        throw "whatsapp.fetch_attachment requires `attachment_id`";
    }
    let r = sidecar_http_get_bytes(
        sidecar_url("wuzapi") + "/chat/downloadimage",
        #{ "url": attachment_id },
        user_headers(),
    );
    #{ "data_url": r.data_url, "mime_type": r.mime_type, "size_bytes": r.size_bytes }
}

// =====================================================================
// HELPERS
// =====================================================================

fn wire_recipient(s) {
    if s == () || s == "" { return s; }
    if s.contains("@") { return s; }
    let digits = s;
    if digits.starts_with("+") {
        digits = digits.sub_string(1);
    }
    digits + "@s.whatsapp.net"
}

fn user_headers() {
    let token = vault_get("user_token");
    if token == () || token == "" {
        throw "whatsapp: no user_token in vault — on_enable hasn't completed yet, or the wuzapi sidecar is down";
    }
    #{ "Token": token }
}

fn summarise_send_response(resp) {
    if resp == () { return "ok"; }
    if type_of(resp) == "map" {
        if resp.data != () && type_of(resp.data) == "map" && resp.data.Details != () {
            return resp.data.Details;
        }
        if resp.Details != () { return resp.Details; }
    }
    "ok"
}

// =====================================================================
// ADMIN ROUTES
// =====================================================================

// GET /api/admin/plugins/whatsapp/status
//
// Reports three distinct states the SPA can present cleanly:
//   * sidecar_status = "down"     -> wuzapi container not running.
//   * sidecar_status = "awaiting_pairing" -> wuzapi up but no
//     user_token in vault (on_enable hasn't finished provisioning,
//     or just after install before the first pairing). The SPA
//     renders the pairing card.
//   * sidecar_status = "healthy"  -> wuzapi up + user provisioned;
//     /session/status fetched cleanly.
//   * sidecar_status = "unhealthy" -> wuzapi up + user provisioned
//     but /session/status threw.
//
// This separation is the fix for the reported bug where a
// "vault empty" condition (a normal first-run state) was being
// reported as "sidecar unhealthy".
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

    // No user provisioned yet → not actually unhealthy. on_enable
    // either hasn't fired (cold-boot race) or its provisioning
    // step failed (admin token mismatch, wuzapi rejecting create).
    // Either way the SPA's pairing card is the right thing to
    // render — it'll keep polling until a token shows up.
    let token = vault_get("user_token");
    if token == () || token == "" {
        return #{
            "sidecar_status": "awaiting_pairing",
            "sidecar_rpc_url": base,
            "registered_accounts": [],
            "accounts_on_disk": [],
            "fetch_error": (),
        };
    }

    let accounts = [];
    let fetch_error = ();
    try {
        let info = sidecar_http_get(base + "/session/status", #{}, #{ "Token": token });
        let data = ();
        if type_of(info) == "map" {
            data = if info.data != () { info.data } else { info };
        }
        if data != () {
            let logged = data.loggedIn == true || data.LoggedIn == true;
            if logged {
                let jid = if data.jid != () { data.jid } else if data.JID != () { data.JID } else { () };
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

// GET /api/admin/plugins/whatsapp/qrcodelink
//
// wuzapi returns the QR as a JSON envelope `{"data": {"QRCode":
// "data:image/png;base64,..."}}` — NOT raw PNG bytes. We extract
// the data_url and pass it through; the SPA puts it directly
// into <img src>.
//
// /session/qr 500s with "no session" if /session/connect was
// never called. Best-effort connect-then-qr in case the boot
// path's connect call failed silently.
fn admin_qrcodelink(req) {
    let base = sidecar_url("wuzapi");
    if base == () {
        return #{ "error": "wuzapi sidecar not running" };
    }
    let token = vault_get("user_token");
    if token == () || token == "" {
        return #{ "error": "wuzapi user not provisioned yet — wait a few seconds and refresh, or check the server log for 'whatsapp: ensure_user_token failed'" };
    }
    // Best-effort connect — idempotent.
    try {
        ensure_session_connected(base);
    } catch (e) {
        return #{ "error": "wuzapi /session/connect failed: " + e };
    }
    try {
        let resp = sidecar_http_get(base + "/session/qr", #{}, #{ "Token": token });
        let data = ();
        if type_of(resp) == "map" {
            data = if resp.data != () { resp.data } else { resp };
        }
        if data == () || data.QRCode == () || data.QRCode == "" {
            return #{ "error": "wuzapi /session/qr returned no QRCode field — payload: " + resp };
        }
        return #{ "data_url": data.QRCode, "mime_type": "image/png" };
    } catch (e) {
        return #{ "error": e.to_string() };
    }
}

// DELETE /api/admin/plugins/whatsapp/unregister-account
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
