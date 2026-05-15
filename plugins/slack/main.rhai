// slack — Slack transport via Socket Mode, multi-workspace, sidecar-free.
//
// Mirrors the post-fix Signal/WhatsApp shape (host owns trust admit
// + routing + auto-bridge; plugin owns the inbound consumer + frame
// decoder + tool dispatch + admin pairing) but with three big
// differences:
//
//   * No sidecar — Slack is public REST + Socket Mode WS. The
//     plugin reaches them through the existing `http_post` and
//     `ws_subscribe` / `ws_send` Rhai bindings.
//
//   * Token auth, not QR pairing. Operator pastes a bot token
//     (xoxb-, REST) + app-level token (xapp-, Socket Mode) per
//     workspace. The SPA's admin form writes them to the per-
//     plugin vault.
//
//   * Multi-workspace from day 1. The vault holds a JSON array
//     of workspace records; on_enable iterates each and opens a
//     Socket Mode connection per workspace. v0.3.0+:
//     admin_add_workspace also connects inline so a fresh
//     workspace starts receiving immediately, without the
//     disable+enable dance the v0.2.0 flow required.
//
// Vault keys
// ----------
//   * `workspaces` -> JSON array of:
//       { team_id, team_name, bot_token, app_token,
//         bot_user_id, controller_user_id }
//   * `channel_workspace` -> JSON map { channel_id: team_id }
//     populated by the inbound consumer; used by outbound tools
//     to pick which workspace owns a given channel.
//   * `channel_thread` -> JSON map { channel_id: thread_ts }
//     populated by the inbound consumer when an event has
//     thread_ts. Outbound replies on the same channel apply this
//     thread_ts so threaded replies stay in-thread without
//     touching the host's InboundMessage struct.

// Note on Rhai scope: top-level `const` declarations aren't visible
// inside `fn` bodies. The Slack REST base URL and message size cap
// are therefore inlined as string/int literals at every call site
// below — same gotcha the google-* plugins document. If you change
// these, grep "slack.com/api" and "3500" together. The references
// here exist as documentation only.
//
//   SLACK_API               = "https://slack.com/api"
//   MAX_SLACK_MESSAGE_CHARS = 3500

// =====================================================================
// LIFECYCLE
// =====================================================================

fn on_enable() {
    let workspaces = load_workspaces();
    if type_of(workspaces) != "array" || workspaces.is_empty() {
        log_info("slack: no workspaces configured yet — visit Settings → Plugins → Slack to add one");
        return;
    }
    for ws in workspaces {
        try {
            connect_workspace(ws);
        } catch (e) {
            log_warn("slack: connect_workspace(" + ws.team_id + ") failed: " + e);
        }
    }
    ()
}

// =====================================================================
// TEARDOWN (2026-05-13)
//
// Fired by the plugin host before disable/uninstall/factory-reset.
// Slack's Socket Mode WS subscriptions are auto-cancelled by the
// host's `plugin.shutdown()` backstop (it walks every live
// `ws_subscribe*` handle and fires its cancel token). on_disable
// here is the audit-log point — confirms the lifecycle hook
// fired so an operator chasing "did factory reset run cleanly?"
// has signal in the host log.
//
// No remote teardown is attempted: Slack doesn't surface a
// "close this Socket Mode subscription" endpoint (the Socket Mode
// URL is single-use and dies on its own once we drop the WS).
// Workspace OAuth tokens stay in vault for the regular disable
// path; factory reset wipes them via the DB-wide table truncate.
// =====================================================================

fn on_disable() {
    let workspaces = load_workspaces();
    let count = if type_of(workspaces) == "array" { workspaces.len() } else { 0 };
    log_info("slack: on_disable — " + count + " workspace WS subscription(s) will be cancelled by host shutdown");
    ()
}

fn connect_workspace(ws) {
    if ws.app_token == () || ws.app_token == "" {
        throw "missing app_token for team " + ws.team_id;
    }
    // apps.connections.open returns a single-use Socket Mode URL.
    let body = #{};
    let resp = http_post(
        "https://slack.com/api/apps.connections.open",
        body,
        ws.app_token,
    );
    if resp == () || resp.ok != true {
        throw "apps.connections.open failed: " + resp;
    }
    let url = resp.url;
    if url == () || url == "" {
        throw "apps.connections.open returned no url";
    }
    log_info("slack: subscribing to Socket Mode for team " + ws.team_id);
    // We pass the team_id through the URL query so on_frame can
    // recover it (each workspace opens its own subscription, but
    // Rhai callbacks don't carry per-handle state). Slack ignores
    // unknown query params.
    let url_with_team = url + "&team_id=" + url_encode(ws.team_id);
    // ws_subscribe_bidi is the bidirectional sibling — its callback
    // receives (handle, frame) so we can ws_send envelope_id ACKs
    // back over the same socket. Slack disconnects in ~3s if any
    // envelope goes unacked.
    let _handle = ws_subscribe_bidi(url_with_team, "on_frame");
    ()
}

// =====================================================================
// INBOUND
// =====================================================================

fn on_frame(handle, raw) {
    // ACK first — Slack disconnects if envelope_id isn't echoed
    // back within ~3s. Then route. Best-effort ACK; if ws_send
    // returns false (disconnected), Slack will redeliver.
    let env;
    try {
        env = parse_json(raw);
    } catch (e) {
        log_warn("slack: parse_json failed: " + e);
        return;
    }
    if env == () { return; }

    // Recover the team_id from the URL fragment we appended in
    // connect_workspace. Slack's WS frames don't include team_id
    // at the envelope level on older shapes; the team_id field
    // may live inside payload. Try both.
    let team_id = env.team_id;
    if team_id == () || team_id == "" {
        if env.payload != () && env.payload.team_id != () {
            team_id = env.payload.team_id;
        }
    }
    // Fall back to the only configured workspace if we still
    // don't know — most installs are single-workspace.
    if team_id == () || team_id == "" {
        let all = load_workspaces();
        if type_of(all) == "array" && all.len() == 1 {
            team_id = all[0].team_id;
        }
    }

    // Slack Socket Mode envelope types: hello, disconnect, events_api, slash_commands, interactive
    let env_type = env.type;
    if env_type == "hello" {
        log_info("slack: hello received for team " + team_id);
        return;
    }
    if env_type == "disconnect" {
        log_info("slack: server-requested disconnect (will reconnect)");
        return;
    }

    // ACK *every* envelope_id, even ones we drop. Slack treats
    // unacked envelopes as failed and re-sends, which would
    // double-deliver every event the agent processes.
    if env.envelope_id != () && env.envelope_id != "" {
        let ack = #{ "envelope_id": env.envelope_id };
        let _ok = ws_send(handle, to_json_string(ack));
        // ws_send returns false on disconnect; we just let Slack's
        // redelivery handle it.
    }

    if env_type != "events_api" { return; }
    let payload = env.payload;
    if payload == () { return; }
    let event = payload.event;
    if event == () || event.type != "message" { return; }
    if event.subtype != () && event.subtype != "" { return; }
    if event.bot_id != () && event.bot_id != "" { return; }
    if event.user == () || event.user == "" { return; }

    // Cache channel → team_id + channel → thread_ts for outbound
    // tools to read.
    if event.channel != () && team_id != () {
        cache_channel_workspace(event.channel, team_id);
    }
    if event.channel != () && event.thread_ts != () && event.thread_ts != "" {
        cache_channel_thread(event.channel, event.thread_ts);
    }

    let inbound = decode_event(team_id, event, payload);
    if inbound == () { return; }
    try {
        host_route_inbound(inbound);
    } catch (e) {
        log_warn("slack: host_route_inbound rejected: " + e);
    }
}

fn decode_event(team_id, event, payload) {
    try {
        let text = if event.text != () { event.text } else { "" };
        // Drop empty messages with no files.
        let has_files = event.files != () && type_of(event.files) == "array" && !event.files.is_empty();
        let trimmed = trim(text);
        if trimmed == "" && !has_files { return (); }

        // Mention-only-in-channels gate. DMs (channel starting
        // with "D" or channel_type=="im") always pass; channel
        // messages need <@SELF> in the text.
        let is_dm = (event.channel_type == "im")
            || (event.channel != () && event.channel.starts_with("D"));
        if !is_dm {
            let bot_user_id = bot_user_id_for_team(team_id);
            if bot_user_id != () && bot_user_id != "" {
                let mention = "<@" + bot_user_id + ">";
                if !text.contains(mention) { return (); }
            }
        }

        // Normalize Slack's link-style mentions for the agent's view.
        let normalized = normalize_slack_text(text, bot_user_id_for_team(team_id));

        // Attachments (images/files). Slack delivers files in the
        // event.files array; we surface url_private as bridge_id.
        let attachments = [];
        if has_files {
            for f in event.files {
                if f.id == () { continue; }
                let url = if f.url_private != () { f.url_private } else { f.permalink };
                if url == () || url == "" { continue; }
                attachments.push(#{
                    "bridge_id": url,
                    "content_type": f.mimetype,
                    "filename": f.name,
                    "size_bytes": f.size,
                });
            }
        }

        let is_group = !is_dm;
        let group_id = if is_group { "slack:" + event.channel } else { () };

        // Slack already gated on `<@bot-user-id>` for non-DM channels
        // above (only mentioning messages reach this point), so any
        // group inbound that escapes the early-return is by definition
        // mentioning the agent. Surface that as a hard verdict to the
        // host's group-addressing layer so it can short-circuit the
        // LLM classifier — explicit mention is never group banter.
        // DMs leave this `()` (1:1 doesn't have an addressing
        // question; the host's eligibility gate already exempts them).
        let mention_of_self = if is_group { true } else { () };

        return #{
            "channel": "slack",
            "native_id": event.user,
            "display_name": (), // resolved server-side via users.info; skip for v0.1
            "group_id": group_id,
            "group_name": (),
            "text": normalized,
            "timestamp_ms": parse_slack_ts_ms(event.ts),
            "attachments": attachments,
            "mention_of_self": mention_of_self,
        };
    } catch (e) {
        log_warn("slack: undecodable event: " + e);
        return ();
    }
}

// =====================================================================
// TOOL DISPATCH
// =====================================================================

fn tool_call(name, args, oauth) {
    if name == "slack.send_message"           { return send_message_impl(args); }
    if name == "slack.reply"                  { return reply_impl(args); }
    if name == "slack.list_channels"          { return list_channels_impl(); }
    if name == "slack.send_with_attachments"  { return send_with_attachments_impl(args); }
    if name == "slack.set_typing"             { return set_typing_impl(args); }
    if name == "slack.fetch_attachment"       { return fetch_attachment_impl(args); }
    throw "unknown tool '" + name + "'";
}

fn send_message_impl(args) {
    let to = args.to;
    let text = args.text;
    if to == () || to == "" { throw "slack.send_message requires `to`"; }
    if text == () || text == "" { throw "slack.send_message requires `text`"; }
    let channel = strip_slack_prefix(to);
    let team_id = lookup_workspace_for_channel(channel);
    if team_id == () || team_id == "" {
        throw "slack.send_message: don't know which workspace owns channel '" + channel + "' — channel must have received an inbound message first, or use a slack:<channel> JID after the bot has joined";
    }
    let bot_token = bot_token_for_team(team_id);
    if bot_token == () || bot_token == "" {
        throw "slack.send_message: no bot_token for team '" + team_id + "'";
    }
    let thread_ts = lookup_channel_thread(channel);
    let chunks = split_slack_message(text);
    let last = ();
    for chunk in chunks {
        let body = #{ "channel": channel, "text": chunk };
        if thread_ts != () && thread_ts != "" { body.thread_ts = thread_ts; }
        last = http_post("https://slack.com/api/chat.postMessage", body, bot_token);
        if last == () || last.ok != true {
            throw "chat.postMessage failed: " + last;
        }
    }
    if last != () && last.ts != () { last.ts.to_string() } else { "ok" }
}

fn reply_impl(args) {
    if args.to == () || args.to == "" {
        throw "slack.reply requires `to` — host fills this from the conversation's binding";
    }
    send_message_impl(args)
}

fn list_channels_impl() {
    let workspaces = load_workspaces();
    let out = [];
    if type_of(workspaces) != "array" { return #{ "channels": [] }; }
    for ws in workspaces {
        let body = #{ "types": "public_channel,private_channel,mpim,im", "limit": 200 };
        let resp = ();
        try {
            resp = http_post("https://slack.com/api/conversations.list", body, ws.bot_token);
        } catch (e) {
            log_warn("slack: conversations.list failed for team " + ws.team_id + ": " + e);
            continue;
        }
        if resp == () || resp.ok != true { continue; }
        let channels = resp.channels;
        if type_of(channels) != "array" { continue; }
        for c in channels {
            if c.id == () { continue; }
            let is_dm = c.is_im == true;
            let name = if c.name != () && c.name != "" {
                "#" + c.name
            } else if is_dm && c.user != () {
                "DM " + c.user
            } else {
                c.id
            };
            out.push(#{
                "id": c.id,
                "name": name,
                "workspace": if ws.team_name != () { ws.team_name } else { ws.team_id },
                "is_dm": is_dm,
            });
        }
    }
    #{ "channels": out, "ack_text": "Listed " + out.len() + " channel(s) across " + workspaces.len() + " workspace(s)." }
}

fn send_with_attachments_impl(args) {
    let to = args.to;
    let text = if args.text != () { args.text } else { "" };
    let attachments = args.attachments;
    if to == () || to == "" { throw "slack.send_with_attachments requires `to`"; }
    if attachments == () || type_of(attachments) != "array" || attachments.is_empty() {
        return send_message_impl(#{ "to": to, "text": text });
    }
    let channel = strip_slack_prefix(to);
    let team_id = lookup_workspace_for_channel(channel);
    if team_id == () || team_id == "" {
        throw "slack.send_with_attachments: unknown workspace for channel '" + channel + "'";
    }
    let bot_token = bot_token_for_team(team_id);
    if bot_token == () || bot_token == "" {
        throw "slack.send_with_attachments: no bot_token for team '" + team_id + "'";
    }
    let thread_ts = lookup_channel_thread(channel);
    let i = 0;
    for aid in attachments {
        let a = host_get_attachment_bytes(aid);
        // 3-step Slack v2 upload:
        //   1. files.getUploadURLExternal — form-encoded! returns {upload_url, file_id}
        //   2. POST raw bytes to upload_url
        //   3. files.completeUploadExternal — JSON, attaches to channel
        //
        // Step 1 must be x-www-form-urlencoded per Slack's own docs;
        // sending JSON gets `invalid_arguments`. We don't have a
        // form-post Rhai binding, so we abuse http_post by setting
        // the body to a pre-encoded string. (TODO: add http_post_form.)
        //
        // For v0.1 — fall back to /chat.postMessage with the data
        // URL embedded in a code block. Better than throwing, but
        // not actual file delivery. Emit a warning so the operator
        // knows attachments aren't fully wired.
        log_warn("slack.send_with_attachments: v0.1 fallback — sending data URL as text. Real files.upload v2 needs an http_post_form binding (follow-up).");
        let body = #{
            "channel": channel,
            "text": (if i == 0 { text + "\n\n" } else { "" })
                + "Attachment " + (i + 1) + " (" + a.mime_type + ", " + a.size_bytes + " bytes): "
                + a.data_url.sub_string(0, 80) + "…",
        };
        if thread_ts != () && thread_ts != "" { body.thread_ts = thread_ts; }
        http_post("https://slack.com/api/chat.postMessage", body, bot_token);
        i += 1;
    }
    "ok"
}

fn set_typing_impl(args) {
    // Slack has no typing-indicator endpoint for bots. The host's
    // typing-guard fires this per turn for cross-channel
    // consistency; we no-op it instead of erroring. `args` is
    // {to, active} — both ignored.
    if args == () { return #{ "ok": true }; }
    #{ "ok": true }
}

fn fetch_attachment_impl(args) {
    let attachment_id = args.attachment_id;
    if attachment_id == () || attachment_id == "" {
        throw "slack.fetch_attachment requires `attachment_id`";
    }
    // attachment_id is the file's url_private. We need the right
    // bot_token to authenticate. Without channel context we fall
    // back to the first workspace's token; correct routing would
    // require carrying team_id through the host's attachment flow.
    let workspaces = load_workspaces();
    if type_of(workspaces) != "array" || workspaces.is_empty() {
        throw "slack.fetch_attachment: no workspaces configured";
    }
    let bot_token = workspaces[0].bot_token;
    // We don't have a "GET with bearer + return bytes" binding
    // beyond sidecar_http_get_bytes (which requires a registered
    // sidecar URL). For v0.1 — surface an error pointing at the
    // follow-up. Research-PDF auto-bridge will surface this if it
    // ever needs to read inbound Slack files.
    throw "slack.fetch_attachment: not yet implemented in v0.1 — needs an http_get_bytes binding with bearer auth (follow-up)";
}

// =====================================================================
// HELPERS
// =====================================================================

// Slack's link conventions in incoming text:
//   <@U…>            -> @user
//   <#C…|name>       -> #name
//   <https://x|text> -> text (https://x)
//   <https://x>      -> https://x
// Self-mention rewritten to @execlaw so the agent sees its name.
fn normalize_slack_text(text, self_user_id) {
    let s = trim(text);
    if s == "" { return ""; }
    // Self-mention first so the more general <@…> sweep doesn't
    // turn it into "@user".
    if self_user_id != () && self_user_id != "" {
        let self_mention = "<@" + self_user_id + ">";
        s.replace(self_mention, "@execlaw");
    }
    // <@USERID> → @user. Rhai's String::replace mutates + returns
    // unit, so we work in a let mut + fold over fixed patterns.
    // Slack user/channel ids are alphanumeric; we strip the literal
    // bracket-form tags one at a time by scanning for "<@", "<#", "<".
    let out = "";
    let i = 0;
    let n = s.len();
    while i < n {
        let ch = s.sub_string(i, 1);
        if ch == "<" && i + 1 < n {
            let next = s.sub_string(i + 1, 1);
            if next == "@" {
                // Find closing '>'
                let close = find_from(s, ">", i);
                if close > i {
                    out += "@user";
                    i = close + 1;
                    continue;
                }
            }
            if next == "#" {
                let close = find_from(s, ">", i);
                if close > i {
                    let inner = s.sub_string(i + 2, close - i - 2);
                    let pipe = inner.index_of("|");
                    let label = if pipe >= 0 { inner.sub_string(pipe + 1) } else { inner };
                    out += "#" + label;
                    i = close + 1;
                    continue;
                }
            }
            // Plain link: <https://x|text> or <https://x>
            let close = find_from(s, ">", i);
            if close > i {
                let inner = s.sub_string(i + 1, close - i - 1);
                let pipe = inner.index_of("|");
                if pipe >= 0 {
                    let url = inner.sub_string(0, pipe);
                    let label = inner.sub_string(pipe + 1);
                    out += label + " (" + url + ")";
                } else {
                    out += inner;
                }
                i = close + 1;
                continue;
            }
        }
        out += ch;
        i += 1;
    }
    trim(out)
}

fn find_from(s, needle, start) {
    let rest = s.sub_string(start);
    let rel = rest.index_of(needle);
    if rel < 0 { return -1; }
    start + rel
}

// Smart paragraph/line/space-aware split at MAX_SLACK_MESSAGE_CHARS.
// Mirrors selfhosted-claw's splitSlackMessage.
fn split_slack_message(text) {
    let trimmed = trim(text);
    if trimmed == "" { return []; }
    if trimmed.len() <= 3500 { return [trimmed]; }
    let out = [];
    let remaining = trimmed;
    while remaining.len() > 3500 {
        let split_at = remaining.index_of("\n\n");
        // index_of returns first; we want last <=3500. Rhai
        // doesn't have last-index helpers — walk forward in
        // chunks to find the rightmost "\n\n" before 3500.
        let cap = 3500;
        let lo = cap / 2;
        let head = remaining.sub_string(0, cap);
        // last_index_of via reverse walk
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
    // Walk left, comparing windowed substrings.
    let pos = i - nlen;
    while pos >= 0 {
        if s.sub_string(pos, nlen) == needle { return pos; }
        if pos == 0 { return -1; }
        pos -= 1;
    }
    -1
}

// Strip a "slack:" prefix the agent might pass; bare channel ids
// pass through unchanged.
fn strip_slack_prefix(s) {
    if s == () || s == "" { return s; }
    if s.starts_with("slack:") { return s.sub_string(6); }
    s
}

// Slack's `ts` field is "<seconds>.<microseconds>" string. Convert
// to an i64 epoch-millis for the InboundMessage timestamp_ms field.
fn parse_slack_ts_ms(ts) {
    if ts == () || ts == "" { return (); }
    let dot = ts.index_of(".");
    if dot < 0 { return (); }
    let secs = ts.sub_string(0, dot);
    let micros = ts.sub_string(dot + 1);
    let s_int = parse_int(secs);
    let m_int = parse_int(micros);
    if s_int == () { return (); }
    let micros_part = if m_int != () { m_int } else { 0 };
    s_int * 1000 + micros_part / 1000
}

fn parse_int(s) {
    if s == () || s == "" { return (); }
    try { return s.to_int(); } catch (e) { return (); }
}

// =====================================================================
// VAULT-BACKED STATE
// =====================================================================

fn load_workspaces() {
    let raw = vault_get("workspaces");
    if raw == () || raw == "" { return []; }
    try { return parse_json(raw); } catch (e) { return []; }
}

fn save_workspaces(arr) {
    vault_put("workspaces", to_json_string(arr));
}

fn bot_token_for_team(team_id) {
    let all = load_workspaces();
    for ws in all {
        if ws.team_id == team_id { return ws.bot_token; }
    }
    ()
}

fn bot_user_id_for_team(team_id) {
    let all = load_workspaces();
    for ws in all {
        if ws.team_id == team_id { return ws.bot_user_id; }
    }
    ()
}

fn cache_channel_workspace(channel, team_id) {
    let raw = vault_get("channel_workspace");
    let m = #{};
    if raw != () && raw != "" {
        try { m = parse_json(raw); } catch (e) { m = #{}; }
        if type_of(m) != "map" { m = #{}; }
    }
    if m[channel] == team_id { return; }
    m[channel] = team_id;
    vault_put("channel_workspace", to_json_string(m));
}

fn lookup_workspace_for_channel(channel) {
    let raw = vault_get("channel_workspace");
    if raw == () || raw == "" {
        // Fallback: single workspace.
        let all = load_workspaces();
        if type_of(all) == "array" && all.len() == 1 {
            return all[0].team_id;
        }
        return ();
    }
    try {
        let m = parse_json(raw);
        if type_of(m) == "map" && m[channel] != () { return m[channel]; }
    } catch (e) {}
    let all = load_workspaces();
    if type_of(all) == "array" && all.len() == 1 { return all[0].team_id; }
    ()
}

fn cache_channel_thread(channel, thread_ts) {
    let raw = vault_get("channel_thread");
    let m = #{};
    if raw != () && raw != "" {
        try { m = parse_json(raw); } catch (e) { m = #{}; }
        if type_of(m) != "map" { m = #{}; }
    }
    m[channel] = thread_ts;
    vault_put("channel_thread", to_json_string(m));
}

fn lookup_channel_thread(channel) {
    let raw = vault_get("channel_thread");
    if raw == () || raw == "" { return (); }
    try {
        let m = parse_json(raw);
        if type_of(m) == "map" { return m[channel]; }
    } catch (e) {}
    ()
}

// =====================================================================
// ADMIN ROUTES
// =====================================================================

fn admin_status(req) {
    let workspaces = load_workspaces();
    let count = if type_of(workspaces) == "array" { workspaces.len() } else { 0 };
    let configured = count > 0;
    #{
        "sidecar_status": if configured { "healthy" } else { "unconfigured" },
        "sidecar_rpc_url": (),
        "registered_accounts": [],
        "accounts_on_disk": [],
        "fetch_error": (),
        "workspaces_configured": count,
    }
}

fn admin_list_workspaces(req) {
    let workspaces = load_workspaces();
    let out = [];
    if type_of(workspaces) == "array" {
        for ws in workspaces {
            out.push(#{
                "team_id": ws.team_id,
                "team_name": ws.team_name,
                "bot_user_id": ws.bot_user_id,
                "controller_user_id": ws.controller_user_id,
                "bot_token_masked": mask(ws.bot_token),
                "app_token_masked": mask(ws.app_token),
            });
        }
    }
    #{ "workspaces": out }
}

// POST /workspaces  body: {bot_token, app_token, controller_user_id?}
//   * Calls auth.test to discover team_id + bot_user_id.
//   * Inserts/updates the workspace row.
//   * Returns {team_id, team_name, bot_user_id, controller_user_id}
//     so the SPA can show what was added.
fn admin_add_workspace(req) {
    if req.body == () { throw "missing request body"; }
    let bot_token = if req.body.bot_token != () { trim(req.body.bot_token) } else { "" };
    let app_token = if req.body.app_token != () { trim(req.body.app_token) } else { "" };
    let controller_user_id = if req.body.controller_user_id != () {
        trim(req.body.controller_user_id)
    } else { "" };
    if bot_token == "" { throw "bot_token required (xoxb-...)"; }
    if app_token == "" { throw "app_token required (xapp-...)"; }
    if !bot_token.starts_with("xoxb-") { throw "bot_token must start with xoxb-"; }
    if !app_token.starts_with("xapp-") { throw "app_token must start with xapp-"; }

    // auth.test → identify the team + bot user.
    let auth = http_post("https://slack.com/api/auth.test", #{}, bot_token);
    if auth == () || auth.ok != true {
        throw "auth.test rejected the bot token: " + auth;
    }
    let team_id = auth.team_id;
    let team_name = auth.team;
    let bot_user_id = auth.user_id;
    if team_id == () || team_id == "" { throw "auth.test returned no team_id"; }
    if bot_user_id == () || bot_user_id == "" { throw "auth.test returned no user_id"; }

    // Probe apps.connections.open so we fail fast if Socket Mode
    // isn't configured / app token is wrong.
    let probe = http_post("https://slack.com/api/apps.connections.open", #{}, app_token);
    if probe == () || probe.ok != true {
        throw "apps.connections.open rejected the app token: " + probe;
    }

    // Upsert into the workspaces array.
    let all = load_workspaces();
    if type_of(all) != "array" { all = []; }
    let updated = [];
    let replaced = false;
    for ws in all {
        if ws.team_id == team_id {
            updated.push(#{
                "team_id": team_id,
                "team_name": team_name,
                "bot_token": bot_token,
                "app_token": app_token,
                "bot_user_id": bot_user_id,
                "controller_user_id": if controller_user_id != "" { controller_user_id } else { ws.controller_user_id },
            });
            replaced = true;
        } else {
            updated.push(ws);
        }
    }
    if !replaced {
        updated.push(#{
            "team_id": team_id,
            "team_name": team_name,
            "bot_token": bot_token,
            "app_token": app_token,
            "bot_user_id": bot_user_id,
            "controller_user_id": controller_user_id,
        });
    }
    save_workspaces(updated);
    log_info("slack: workspace " + team_id + " (" + team_name + ") saved");

    // v0.3.0: open the Socket Mode subscription inline so the
    // operator doesn't have to disable+re-enable the plugin to
    // start receiving messages on a freshly-saved workspace. The
    // old behaviour was a "saved but silent" footgun — tokens
    // landed in vault but on_enable() had already run with zero
    // workspaces and never re-fired.
    //
    // Find the row we just upserted in `updated` (auth.test wrote
    // the canonical team_id / bot_user_id, so prefer that copy over
    // the request body) and connect. Failure to connect doesn't
    // unwind the save — the tokens are valid (we just probed
    // apps.connections.open above), so a transient WS failure
    // should leave the persisted workspace intact and surface as a
    // `connection_warning` the SPA can show. on_enable() at next
    // plugin reload will retry.
    //
    // Replacing an existing workspace (token rotation): the old
    // workspace's WS subscription stays alive in the script
    // engine's subscription registry. Slack's server side will
    // close it shortly after we stop ACKing on a fresh URL;
    // accepting that minor overlap is fine for the rare token-
    // rotation case.
    let saved_ws = #{};
    for ws in updated {
        if ws.team_id == team_id {
            saved_ws = ws;
            break;
        }
    }
    let connection_warning = ();
    try {
        connect_workspace(saved_ws);
    } catch (e) {
        log_warn("slack: connect_workspace(" + team_id + ") after save failed: " + e);
        connection_warning = "" + e;
    }

    let resp = #{
        "team_id": team_id,
        "team_name": team_name,
        "bot_user_id": bot_user_id,
        "controller_user_id": controller_user_id,
    };
    if connection_warning != () {
        resp["connection_warning"] = connection_warning;
    }
    resp
}

fn admin_remove_workspace(req) {
    let team_id = ();
    if req.query != () && req.query.team_id != () {
        team_id = trim(req.query.team_id);
    }
    if team_id == () || team_id == "" { throw "missing required query param 'team_id'"; }
    let all = load_workspaces();
    let kept = [];
    if type_of(all) == "array" {
        for ws in all {
            if ws.team_id != team_id { kept.push(ws); }
        }
    }
    save_workspaces(kept);
    #{ "ok": true }
}

fn admin_test_message(req) {
    if req.body == () { throw "missing request body"; }
    let team_id = if req.body.team_id != () { req.body.team_id } else { "" };
    let channel = if req.body.channel != () { req.body.channel } else { "" };
    if team_id == "" { throw "team_id required"; }
    if channel == "" { throw "channel required"; }
    let bot_token = bot_token_for_team(team_id);
    if bot_token == () || bot_token == "" {
        throw "no bot_token for team '" + team_id + "'";
    }
    let resp = http_post("https://slack.com/api/chat.postMessage", #{
        "channel": strip_slack_prefix(channel),
        "text": "execlaw test message — Slack plugin v0.1.0 wiring confirmed.",
    }, bot_token);
    if resp == () || resp.ok != true {
        return #{ "ok": false, "error": if resp != () && resp.error != () { resp.error } else { "unknown" } };
    }
    #{ "ok": true, "ts": resp.ts }
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
