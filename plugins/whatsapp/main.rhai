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

// USER_TOKEN_NAME = "execlaw-user" — inlined at every call site
// (Rhai's module-level `const` doesn't reach into fn bodies).

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
    // QR code instead of 500-ing with "no session". The
    // ensure_session_connected helper swallows wuzapi's HTTP 500
    // "already connected" response since that's the desired state.
    try {
        ensure_session_connected(base);
    } catch (e) {
        log_warn("whatsapp: /session/connect failed: " + e + " — pairing UI will retry on each poll");
    }
    // wuzapi has no WebSocket event surface (verified upstream:
    // routes.go has /session/qr / /webhook / /chat/* but no /ws,
    // and the surveyed alternatives — WAHA / WPPConnect /
    // Evolution-API — all use Socket.IO which our raw-WS client
    // can't speak). Event delivery flows through wuzapi's
    // webhook: we POST a callback URL to wuzapi /webhook, then
    // wuzapi POSTs every Message event to our
    // /api/webhooks/whatsapp/event handler.
    //
    // The callback URL embeds a secret token validated inside
    // the webhook handler. Without the token check the endpoint
    // is reachable from anyone on the bind address and could be
    // used to inject fake inbound messages. Generated once at
    // first enable and persisted in vault.
    try {
        ensure_webhook_registered(base);
    } catch (e) {
        log_warn("whatsapp: webhook registration failed: " + e + " — inbound messages won't reach the agent until this resolves");
    }
    ()
}

// Generate (or reuse) a webhook secret + register the callback
// URL with wuzapi so it knows where to POST inbound events.
//
// Idempotent: on every plugin enable we re-POST so a wuzapi
// container restart that lost its config gets re-armed. Wuzapi's
// POST /webhook expects the field names `webhookurl` (one word)
// and `events`, per upstream `webhookStruct` in handlers.go.
// Sending `webhook`/`subscribe` is silently accepted but writes
// empty strings to the DB — webhook never fires.
fn ensure_webhook_registered(base) {
    // Generate the secret once, persist for life of the install.
    let secret = vault_get("webhook_secret");
    if secret == () || secret == "" {
        // hash() is deterministic but takes the timestamp as input,
        // giving us a 64-char hex string per fresh install. Good
        // enough for a path-only secret — if an attacker can guess
        // a 256-bit hash they can also guess the api_key.
        secret = hash(now_ms().to_string() + "-" + "execlaw-user");
        vault_put("webhook_secret", secret);
        log_info("whatsapp: minted webhook secret");
    }
    // Public callback URL. The host's bind address might not be
    // 127.0.0.1:3031 in every install — but the wuzapi sidecar
    // runs in Docker and reaches the host via host.docker.internal
    // OR the container's gateway. For now hardcode the localhost
    // bind; future improvement: read from config_general.bind_address.
    let webhook_url = "http://host.docker.internal:3031/api/webhooks/whatsapp/event?token=" + secret;
    let body = #{
        "webhookurl": webhook_url,
        "events": ["Message"],
    };
    sidecar_http_post(base + "/webhook", body, user_headers());
    log_info("whatsapp: webhook registered at " + webhook_url.sub_string(0, 50) + "...");
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

// POST /session/connect. Required before /session/qr will return
// a QR; that endpoint 500s with "no session" without it.
//
// Wuzapi is NOT actually idempotent on this endpoint despite the
// previous comment claiming so — a second /session/connect on an
// already-connected session returns HTTP 500 with body
// `{"error":"already connected"}`. We catch that specific error
// and treat it as success, since "already connected" is exactly
// what we want. Any OTHER 500 (genuine sidecar fault, missing
// auth, etc.) re-throws.
fn ensure_session_connected(base) {
    try {
        sidecar_http_post(base + "/session/connect", #{
            "Subscribe": ["Message"],
            "Immediate": true,
        }, user_headers());
    } catch (e) {
        // Stringify via concatenation — `e` may be a Rhai Map
        // (errors from runtime calls come through as ErrorRuntime
        // structs); `e.to_string()` isn't guaranteed to be
        // registered, but `"" + e` forces the engine's display
        // path which is.
        let msg = "" + e;
        if msg.contains("already connected") {
            // Session is already up — that's the desired state.
            // /session/qr will work for unpaired sessions; for
            // already-paired sessions the caller should check
            // status before invoking the QR flow.
            log_info("whatsapp: session already connected — skipping connect (no-op)");
            return;
        }
        throw e;
    }
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

// =====================================================================
// WEBHOOK
// =====================================================================
//
// Mounted at `POST /api/webhooks/whatsapp/event` per the
// `[[webhook_routes]]` decl in plugin.toml. Wuzapi POSTs every
// Message event here as JSON. The handler:
//
//   1. Validates `?token=<secret>` matches the vault-stored
//      webhook_secret (rejects 401 on mismatch). Without this any
//      caller on the bind address could inject fake messages.
//   2. Decodes the wuzapi event payload into the channel-agnostic
//      InboundMessage shape.
//   3. Calls host_route_inbound to fan out through the standard
//      pipeline (trust admit, conversation routing, agent dispatch,
//      etc.).
//
// Returns `{ok: true}` on accepted events, `{ok: false, error: ...}`
// on auth / decode failure. Wuzapi treats non-200 as a delivery
// failure and may retry; we always respond 200 with an envelope so
// retries don't compound.
fn on_webhook_event(args) {
    let supplied = ();
    if args.query != () && args.query.token != () {
        supplied = args.query.token;
    }
    let expected = vault_get("webhook_secret");
    if expected == () || expected == "" {
        log_warn("whatsapp: webhook hit but no secret in vault — refusing (re-enable the plugin to mint a fresh secret)");
        return #{ "ok": false, "error": "webhook_secret_missing" };
    }
    if supplied == () || supplied != expected {
        log_warn("whatsapp: webhook token mismatch from caller; rejecting");
        return #{ "ok": false, "error": "invalid_token" };
    }
    let body = args.body;
    if body == () || type_of(body) != "map" {
        return #{ "ok": false, "error": "missing_body" };
    }
    // wuzapi wraps the actual whatsmeow event in a string-typed
    // `jsonData` field on an envelope `{instanceName, jsonData,
    // userID}`. Verified from the upstream container logs:
    //   webhookData={"instanceName":"...","jsonData":"{\"event\":{...},\"type\":\"Message\"}","userID":"..."}
    // Our `decode_event_map` expects the *inner* object — parse
    // jsonData first if present, else fall back to treating the
    // body as the inner shape (covers wuzapi forks that flatten).
    let event_map = body;
    if body.jsonData != () && type_of(body.jsonData) == "string" {
        try {
            event_map = parse_json(body.jsonData);
        } catch (e) {
            log_warn("whatsapp: webhook jsonData unparseable: " + e);
            return #{ "ok": false, "error": "unparseable_jsonData" };
        }
    }
    let inbound = decode_event_map(event_map);
    if inbound == () {
        // Most events are non-content (read receipts, presence,
        // history sync chunks). decode_event_map returns Unit for
        // those — accept and ack so wuzapi doesn't retry.
        return #{ "ok": true };
    }
    // Enrich with the group's user-facing name when this is a
    // group inbound. Wuzapi's per-message webhook payload doesn't
    // include the group title — only the JID. Without a name the
    // host's auto-rename leaves the conversation display_name
    // empty AND the agent's per-turn group-context block falls
    // back to "this group" instead of the real title. Look it up
    // through `/group/list` and cache so subsequent inbounds for
    // the same group don't pay the round-trip.
    if inbound.group_id != () && inbound.group_id != "" {
        try {
            let name = resolve_group_name_cached(inbound.group_id);
            if name != () && name != "" {
                inbound["group_name"] = name;
            }
        } catch (e) {
            // Best-effort: a name-resolution failure must not drop
            // the inbound. The agent loses the group title for this
            // turn but the rest of the pipeline runs.
            log_warn("whatsapp: resolve_group_name failed for " + inbound.group_id + ": " + e);
        }
    }
    try {
        host_route_inbound(inbound);
    } catch (e) {
        log_warn("whatsapp: host_route_inbound rejected: " + e);
    }
    #{ "ok": true }
}

// Resolve a WhatsApp group JID to its user-facing title, with a
// vault-backed cache so we hit /group/list at most once per group
// per process lifetime (the cache is keyed in the vault, so
// installs survive restarts too).
//
// On miss: fetches the full group list, populates the cache for
// EVERY group in the response (not just the one we asked for —
// the round-trip cost is paid either way and pre-warming saves
// work on the next group's first inbound), and returns the match.
//
// Returns `()` when the lookup can't find the group (e.g. wuzapi
// not yet aware of it, or a JID for a group the controller
// recently left). Caller is expected to handle `()` — the
// webhook handler does.
//
// Vault key shape: `group_name:<jid>`. Stored as a plain string;
// no TTL — group renames are rare enough that this can age
// gracefully, and a future `whatsapp.refresh_group_names` admin
// tool can blow the cache when a rename surfaces.
fn resolve_group_name_cached(group_jid) {
    let cache_key = "group_name:" + group_jid;
    let cached = vault_get(cache_key);
    if cached != () && cached != "" {
        return cached;
    }
    // Cache miss — fetch /group/list.
    let listed = list_groups_impl();
    if listed == () || listed.groups == () { return (); }
    let hit = ();
    for g in listed.groups {
        if g.id == () || g.name == () || g.name == "" { continue; }
        // Cache the name for every returned group so the next
        // inbound on a sibling group is free.
        vault_put("group_name:" + g.id, g.name);
        if g.id == group_jid { hit = g.name; }
    }
    hit
}

// Pure helper: decode a wuzapi WS event into the channel-agnostic
// InboundMessage shape OR `()` for non-content events.
//
// Kept against the raw-string path even though we no longer have
// a WS event surface — the tests in
// `crates/script/tests/whatsapp_plugin.rs` (when they exist) call
// this with raw JSON strings. The webhook code path goes through
// `decode_event_map` directly because the body is already a Map
// by the time the host's webhook dispatcher calls our handler.
fn decode_frame(raw) {
    try {
        let env = parse_json(raw);
        if env == () { return (); }
        return decode_event_map(env);
    } catch (e) {
        log_warn("whatsapp: undecodable frame: " + e);
        return ();
    }
}

// Pure helper: same logic as decode_frame but takes an
// already-parsed Map. Webhook handler uses this directly because
// the host's webhook dispatcher passes `body` as a Map (decoded
// from the wuzapi POST's JSON body).
fn decode_event_map(env) {
    try {
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
        log_warn("whatsapp: undecodable event: " + e);
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

    // Opportunistically re-register the webhook on every status
    // poll. on_enable's attempt may have failed (wuzapi was
    // cycling) and the SPA polls /status every few seconds, so
    // this gives us a natural retry rhythm without needing a
    // dedicated background task.
    try {
        ensure_webhook_registered(base);
    } catch (e) {
        // Non-fatal — log and let the next poll retry.
        log_warn("whatsapp: webhook re-register from status failed: " + e);
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
    // Retry webhook registration here too — on_enable's attempt
    // may have hit a wuzapi-cycling Network Error and given up.
    // Operator clicking the QR-pair button is a natural retry
    // moment; if wuzapi is up enough to serve the QR, it's up
    // enough to accept the webhook config.
    try {
        ensure_webhook_registered(base);
    } catch (e) {
        log_warn("whatsapp: webhook re-register from qrcodelink failed: " + e);
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
