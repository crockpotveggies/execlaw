// google-apps — consolidated Google plugin (Gmail, Calendar, Contacts,
// Tasks, Drive). Replaces the separate google-calendar +
// google-contacts plugins; adds Gmail, Tasks, Drive.
//
// Dispatch: `tool_call(name, args, oauth)` parses the `<module>.<verb>`
// prefix and routes to the matching impl. Each module can be toggled
// on/off via the `enabled_modules` vault setting — the toggle gates
// dispatch but does NOT shrink the OAuth scope grant (v1 ships union
// scopes; per-scope incrementality is v0.2).
//
// `oauth` is a map of {account_name: access_token} the host already
// injected from state_oauth_tokens. We never see the refresh_token or
// client_secret — those stay on the server, rotated by the OAuth
// sweeper.
//
// `identity_resolve(transport, handle, oauth)` provides Google
// Contacts identity-provider behaviour. When the contacts module is
// disabled it returns no match so the host's identity-provider chain
// falls through.
//
// Note on Rhai scope: top-level `const` declarations aren't visible
// inside `fn` bodies, so module names + URL prefixes are inlined
// inside the helpers that need them.

// ---------------------------------------------------------------------------
// Module toggle gate.

// Default: all five modules on. Operators turn modules off via
// PUT /api/admin/plugins/google-apps/settings/enabled_modules — the
// host writes through vault_put. Reads happen on every dispatch (so
// a toggle change takes effect on the next tool call without a
// plugin reload).
fn is_module_enabled(mod_name) {
    // Default to all-on whenever we can't determine the setting:
    //   * vault_get returns () → never persisted, fresh install.
    //   * vault_get throws → script-tier test fixtures don't wire
    //     vault capabilities. Failing closed here would break every
    //     dispatch test that doesn't bother setting the toggle.
    //   * JSON parse fails → malformed value. SPA validates on save,
    //     so this is a defensive fallback rather than a real path.
    //
    // The vault_get + parse_json calls live in their own try-block
    // pair so we can distinguish "host caps absent" (tests) from a
    // real "module is disabled" answer.
    try {
        let raw = vault_get("enabled_modules");
        if raw == () {
            return true;
        }
        let parsed = parse_json(raw);
        if type_of(parsed) != "array" {
            return true;
        }
        for m in parsed {
            // Coerce both sides to strings via concatenation — Rhai's
            // `==` on `Dynamic` versus a `String` literal is finicky
            // when the parse_json result wraps each element in
            // Dynamic. Stringifying makes the comparison literal-
            // safe.
            if ("" + m) == ("" + mod_name) {
                return true;
            }
        }
        return false;
    } catch (err) {
        return true;
    }
}

fn module_for_tool(name) {
    if name.starts_with("gmail.") { return "gmail"; }
    if name.starts_with("calendar.") { return "calendar"; }
    if name.starts_with("contacts.") { return "contacts"; }
    if name.starts_with("tasks.") { return "tasks"; }
    if name.starts_with("drive.") { return "drive"; }
    ""
}

fn require_module(name) {
    let mod_name = module_for_tool(name);
    if mod_name == "" {
        throw "unknown tool '" + name + "'";
    }
    if !is_module_enabled(mod_name) {
        throw "google-apps: " + mod_name + " module is disabled — visit Settings → Plugins → google-apps to enable it";
    }
}

// ---------------------------------------------------------------------------
// Public dispatch surface.

fn tool_call(name, args, oauth) {
    let token = oauth["controller"];
    if token == () {
        throw "Google account not connected — visit Settings → Plugins → google-apps to configure";
    }
    require_module(name);

    // Gmail
    if name == "gmail.list_messages" { return gmail_list_messages_impl(token, args); }
    if name == "gmail.search" { return gmail_search_impl(token, args); }
    if name == "gmail.get_message" { return gmail_get_message_impl(token, args); }
    if name == "gmail.send_message" { return gmail_send_message_impl(token, args); }
    if name == "gmail.create_draft" { return gmail_create_draft_impl(token, args); }
    if name == "gmail.reply" { return gmail_reply_impl(token, args); }
    if name == "gmail.list_labels" { return gmail_list_labels_impl(token); }
    if name == "gmail.add_label" { return gmail_modify_labels_impl(token, args, true); }
    if name == "gmail.remove_label" { return gmail_modify_labels_impl(token, args, false); }
    if name == "gmail.trash" { return gmail_trash_impl(token, args); }

    // Calendar
    if name == "calendar.list_calendars" { return cal_list_calendars_impl(token); }
    if name == "calendar.list_events" { return cal_list_events_impl(token, args); }
    if name == "calendar.check_availability" { return cal_check_availability_impl(token, args); }
    if name == "calendar.get_event" { return cal_get_event_impl(token, args); }
    if name == "calendar.create_event" { return cal_create_event_impl(token, args); }
    if name == "calendar.update_event" { return cal_update_event_impl(token, args); }
    if name == "calendar.delete_event" { return cal_delete_event_impl(token, args); }

    // Contacts
    if name == "contacts.list" { return contacts_list_impl(token, args); }

    // Tasks
    if name == "tasks.list_lists" { return tasks_list_lists_impl(token); }
    if name == "tasks.list_tasks" { return tasks_list_tasks_impl(token, args); }
    if name == "tasks.create_task" { return tasks_create_task_impl(token, args); }
    if name == "tasks.update_task" { return tasks_update_task_impl(token, args); }
    if name == "tasks.complete_task" { return tasks_complete_task_impl(token, args); }
    if name == "tasks.delete_task" { return tasks_delete_task_impl(token, args); }

    // Drive
    if name == "drive.search" { return drive_search_impl(token, args); }
    if name == "drive.get_file_metadata" { return drive_get_file_metadata_impl(token, args); }
    if name == "drive.list_folder" { return drive_list_folder_impl(token, args); }
    if name == "drive.read_file" { return drive_read_file_impl(token, args); }
    if name == "drive.create_file" { return drive_create_file_impl(token, args); }
    if name == "drive.share" { return drive_share_impl(token, args); }

    throw "unknown tool '" + name + "'";
}

// identity_resolve — same shape as plugins/google-contacts. Gated on
// the contacts module toggle so disabling contacts also disables
// identity resolution (the host's chain falls through).
fn identity_resolve(transport, handle, oauth) {
    let token = oauth["controller"];
    if token == () {
        return #{ "match": () };
    }
    if !is_module_enabled("contacts") {
        return #{ "match": () };
    }
    if transport != "email" && transport != "phone" {
        return #{ "match": () };
    }
    let target = if transport == "email" {
        lower(trim(handle))
    } else {
        digits_only(handle)
    };
    let connections = contacts_fetch_connections(token);
    if connections == () {
        return #{ "match": () };
    }
    for person in connections {
        if contacts_person_matches(person, transport, target) {
            return #{ "match": contacts_build_match(person) };
        }
    }
    #{ "match": () }
}

// ===========================================================================
// Gmail
// ===========================================================================

// Decoded message metadata shape used by list/search/get. Pulled out
// so we can summarise both list_messages results (after a second
// get-for-each fetch) and a single get_message response with one
// helper.
fn gmail_summarize_message(msg) {
    let id = if msg["id"] != () { msg["id"] } else { "" };
    let thread_id = if msg["threadId"] != () { msg["threadId"] } else { "" };
    let snippet = if msg["snippet"] != () { msg["snippet"] } else { "" };
    let label_ids = if msg["labelIds"] != () && type_of(msg["labelIds"]) == "array" {
        msg["labelIds"]
    } else {
        []
    };
    let from = "";
    let to = "";
    let subject = "";
    let date = "";
    let payload = msg["payload"];
    if payload != () && payload["headers"] != () {
        for h in payload["headers"] {
            let n = if h["name"] != () { h["name"] } else { "" };
            let v = if h["value"] != () { h["value"] } else { "" };
            let nl = lower(n);
            if nl == "from" { from = v; }
            else if nl == "to" { to = v; }
            else if nl == "subject" { subject = v; }
            else if nl == "date" { date = v; }
        }
    }
    let body_text = gmail_extract_text_body(payload);
    let out = #{
        "id": id,
        "thread_id": thread_id,
        "from": from,
        "to": to,
        "subject": subject,
        "snippet": snippet,
        "date": date,
        "label_ids": label_ids,
    };
    if body_text != () {
        out["body"] = body_text;
    }
    out
}

// Walks a Gmail payload tree and returns the first text/plain part's
// decoded body, or () when none. Gmail nests parts arbitrarily for
// multipart/alternative + multipart/mixed.
fn gmail_extract_text_body(payload) {
    if payload == () {
        return ();
    }
    let mime = if payload["mimeType"] != () { payload["mimeType"] } else { "" };
    let body = payload["body"];
    if mime == "text/plain" && body != () && body["data"] != () {
        let dec = base64_decode(body["data"]);
        if dec != () {
            return dec;
        }
    }
    if payload["parts"] != () && type_of(payload["parts"]) == "array" {
        for p in payload["parts"] {
            let inner = gmail_extract_text_body(p);
            if inner != () {
                return inner;
            }
        }
    }
    ()
}

fn gmail_list_messages_impl(token, args) {
    let max_results = if args["max_results"] != () {
        let n = args["max_results"];
        if n > 100 { 100 } else if n < 1 { 1 } else { n }
    } else {
        25
    };
    let query = #{ "maxResults": "" + max_results };
    if args["q"] != () && args["q"] != "" {
        query["q"] = args["q"];
    }
    if args["label_ids"] != () && type_of(args["label_ids"]) == "array" {
        // Gmail accepts repeated `labelIds=` params; we only support
        // a single string concatenation here (ureq's query encoder
        // doesn't expose repeated keys). The agent rarely needs more
        // than one label filter at a time.
        let joined = "";
        let first = true;
        for lid in args["label_ids"] {
            if first {
                joined = "" + lid;
                first = false;
            } else {
                // Subsequent label ids use Gmail search syntax via
                // `q` — fall back to that when more than one supplied.
                let extra = "label:" + lid;
                if query["q"] != () {
                    query["q"] = "" + query["q"] + " " + extra;
                } else {
                    query["q"] = extra;
                }
            }
        }
        if joined != "" {
            query["labelIds"] = joined;
        }
    }

    let resp = http_get(
        "https://gmail.googleapis.com/gmail/v1/users/me/messages",
        query,
        token,
    );
    if resp == () || resp["messages"] == () {
        return #{ "messages": [] };
    }
    // Follow-up GET each id with format=metadata to get the headers
    // the agent needs. Gmail's list endpoint returns id + threadId
    // only — useless on its own.
    let out = [];
    for stub in resp["messages"] {
        let id = if stub["id"] != () { stub["id"] } else { "" };
        if id == "" { continue; }
        let url = "https://gmail.googleapis.com/gmail/v1/users/me/messages/"
            + url_encode(id) + "?format=metadata";
        let detail = http_get(url, #{}, token);
        if detail == () { continue; }
        let summary = gmail_summarize_message(detail);
        // metadata-format doesn't include a decoded body — strip the
        // empty `body` field if extract returned () so the shape is
        // predictable.
        out.push(summary);
    }
    let result = #{ "messages": out };
    if resp["nextPageToken"] != () {
        result["next_page_token"] = resp["nextPageToken"];
    }
    result
}

fn gmail_search_impl(token, args) {
    let query = if args["query"] != () { args["query"] } else { "" };
    if query == "" {
        throw "gmail.search requires `query`";
    }
    let forwarded = #{ "q": query };
    if args["max_results"] != () {
        forwarded["max_results"] = args["max_results"];
    }
    gmail_list_messages_impl(token, forwarded)
}

fn gmail_get_message_impl(token, args) {
    let id = if args["id"] != () { args["id"] } else { "" };
    if id == "" {
        throw "gmail.get_message requires `id`";
    }
    let format = if args["format"] != () && args["format"] != "" {
        args["format"]
    } else {
        "full"
    };
    let url = "https://gmail.googleapis.com/gmail/v1/users/me/messages/"
        + url_encode(id) + "?format=" + url_encode(format);
    let resp = http_get(url, #{}, token);
    if resp == () {
        throw "gmail.get_message: empty response from Google";
    }
    gmail_summarize_message(resp)
}

// Build an RFC822 message and base64url-encode it. Headers preserved
// in canonical order; body is text/plain UTF-8 to keep things
// predictable across mail clients.
fn gmail_build_raw_message(args, extra_headers) {
    let to_field = gmail_format_address_list(args["to"]);
    if to_field == "" {
        throw "gmail.send_message requires `to`";
    }
    let subject = if args["subject"] != () { args["subject"] } else { "" };
    let body = if args["body"] != () { args["body"] } else { "" };
    let cc = gmail_format_address_list(args["cc"]);
    let bcc = gmail_format_address_list(args["bcc"]);

    let lines = "To: " + to_field + "\r\n";
    if cc != "" { lines += "Cc: " + cc + "\r\n"; }
    if bcc != "" { lines += "Bcc: " + bcc + "\r\n"; }
    lines += "Subject: " + subject + "\r\n";
    lines += "MIME-Version: 1.0\r\n";
    lines += "Content-Type: text/plain; charset=UTF-8\r\n";
    for h in extra_headers {
        lines += h["name"] + ": " + h["value"] + "\r\n";
    }
    lines += "\r\n" + body;
    base64url_encode(lines)
}

fn gmail_format_address_list(value) {
    if value == () { return ""; }
    if type_of(value) == "array" {
        let acc = "";
        let first = true;
        for v in value {
            if first { acc = "" + v; first = false; }
            else { acc += ", " + v; }
        }
        return acc;
    }
    "" + value
}

fn gmail_send_message_impl(token, args) {
    let extra = [];
    let thread_id = ();
    if args["in_reply_to_message_id"] != () && args["in_reply_to_message_id"] != "" {
        // Look up source message to extract Message-ID header + thread.
        let src_id = args["in_reply_to_message_id"];
        let url = "https://gmail.googleapis.com/gmail/v1/users/me/messages/"
            + url_encode(src_id) + "?format=metadata";
        let src = http_get(url, #{}, token);
        if src != () {
            if src["threadId"] != () {
                thread_id = src["threadId"];
            }
            if src["payload"] != () && src["payload"]["headers"] != () {
                for h in src["payload"]["headers"] {
                    if h["name"] != () && lower(h["name"]) == "message-id" {
                        extra.push(#{
                            "name": "In-Reply-To",
                            "value": h["value"],
                        });
                        extra.push(#{
                            "name": "References",
                            "value": h["value"],
                        });
                    }
                }
            }
        }
    }
    let raw = gmail_build_raw_message(args, extra);
    let body = #{ "raw": raw };
    if thread_id != () {
        body["threadId"] = thread_id;
    }
    let resp = http_post(
        "https://gmail.googleapis.com/gmail/v1/users/me/messages/send",
        body,
        token,
    );
    if resp == () {
        throw "gmail.send_message: empty response from Google";
    }
    #{
        "id": if resp["id"] != () { resp["id"] } else { "" },
        "thread_id": if resp["threadId"] != () { resp["threadId"] } else { "" },
    }
}

fn gmail_create_draft_impl(token, args) {
    let raw = gmail_build_raw_message(args, []);
    let body = #{ "message": #{ "raw": raw } };
    let resp = http_post(
        "https://gmail.googleapis.com/gmail/v1/users/me/drafts",
        body,
        token,
    );
    if resp == () {
        throw "gmail.create_draft: empty response from Google";
    }
    let draft_id = if resp["id"] != () { resp["id"] } else { "" };
    let message_id = "";
    if resp["message"] != () && resp["message"]["id"] != () {
        message_id = resp["message"]["id"];
    }
    #{ "id": draft_id, "message_id": message_id }
}

fn gmail_reply_impl(token, args) {
    let src_id = if args["message_id"] != () { args["message_id"] } else { "" };
    if src_id == "" {
        throw "gmail.reply requires `message_id`";
    }
    let body = if args["body"] != () { args["body"] } else { "" };
    // Fetch the source for Subject + To + Message-ID.
    let url = "https://gmail.googleapis.com/gmail/v1/users/me/messages/"
        + url_encode(src_id) + "?format=metadata";
    let src = http_get(url, #{}, token);
    if src == () {
        throw "gmail.reply: source message not found";
    }
    let reply_to = "";
    let subject = "";
    let message_id_header = "";
    if src["payload"] != () && src["payload"]["headers"] != () {
        for h in src["payload"]["headers"] {
            if h["name"] != () {
                let nl = lower(h["name"]);
                if nl == "reply-to" { reply_to = h["value"]; }
                else if nl == "from" && reply_to == "" { reply_to = h["value"]; }
                else if nl == "subject" { subject = h["value"]; }
                else if nl == "message-id" { message_id_header = h["value"]; }
            }
        }
    }
    if !subject.starts_with("Re:") && !subject.starts_with("RE:") {
        subject = "Re: " + subject;
    }
    let send_args = #{
        "to": reply_to,
        "subject": subject,
        "body": body,
    };
    let extra = [];
    if message_id_header != "" {
        extra.push(#{ "name": "In-Reply-To", "value": message_id_header });
        extra.push(#{ "name": "References", "value": message_id_header });
    }
    let raw = gmail_build_raw_message(send_args, extra);
    let post_body = #{ "raw": raw };
    if src["threadId"] != () {
        post_body["threadId"] = src["threadId"];
    }
    let resp = http_post(
        "https://gmail.googleapis.com/gmail/v1/users/me/messages/send",
        post_body,
        token,
    );
    if resp == () {
        throw "gmail.reply: empty response from Google";
    }
    #{
        "id": if resp["id"] != () { resp["id"] } else { "" },
        "thread_id": if resp["threadId"] != () { resp["threadId"] } else { "" },
    }
}

fn gmail_list_labels_impl(token) {
    let resp = http_get_cached(
        "https://gmail.googleapis.com/gmail/v1/users/me/labels",
        #{},
        token,
        300,
    );
    if resp == () || resp["labels"] == () {
        return #{ "labels": [] };
    }
    let out = [];
    for lbl in resp["labels"] {
        out.push(#{
            "id": if lbl["id"] != () { lbl["id"] } else { "" },
            "name": if lbl["name"] != () { lbl["name"] } else { "" },
            "type": if lbl["type"] != () { lbl["type"] } else { "user" },
        });
    }
    #{ "labels": out }
}

fn gmail_modify_labels_impl(token, args, add) {
    let msg_id = if args["message_id"] != () { args["message_id"] } else { "" };
    if msg_id == "" {
        throw "gmail.add_label/remove_label requires `message_id`";
    }
    let label_ids = if args["label_ids"] != () && type_of(args["label_ids"]) == "array" {
        args["label_ids"]
    } else {
        []
    };
    if label_ids.len() == 0 {
        throw "gmail.add_label/remove_label requires non-empty `label_ids`";
    }
    let body = if add {
        #{ "addLabelIds": label_ids }
    } else {
        #{ "removeLabelIds": label_ids }
    };
    let url = "https://gmail.googleapis.com/gmail/v1/users/me/messages/"
        + url_encode(msg_id) + "/modify";
    let resp = http_post(url, body, token);
    if resp == () {
        return #{ "modified": true, "message_id": msg_id };
    }
    #{
        "modified": true,
        "message_id": msg_id,
        "label_ids": if resp["labelIds"] != () { resp["labelIds"] } else { [] },
    }
}

fn gmail_trash_impl(token, args) {
    let msg_id = if args["message_id"] != () { args["message_id"] } else { "" };
    if msg_id == "" {
        throw "gmail.trash requires `message_id`";
    }
    let url = "https://gmail.googleapis.com/gmail/v1/users/me/messages/"
        + url_encode(msg_id) + "/trash";
    // POST with empty body — Gmail's trash endpoint takes no payload.
    http_post(url, #{}, token);
    #{ "trashed": true, "message_id": msg_id }
}

// ===========================================================================
// Calendar — ported from plugins/google-calendar/main.rhai (verbatim
// logic, helpers prefixed with `cal_` to avoid collisions).
// ===========================================================================

fn cal_list_calendars_impl(token) {
    let resp = http_get_cached(
        "https://www.googleapis.com/calendar/v3/users/me/calendarList",
        #{},
        token,
        300,
    );
    if resp == () || resp["items"] == () {
        return #{ "calendars": [] };
    }
    let out = [];
    for item in resp["items"] {
        out.push(cal_summarize_calendar(item));
    }
    #{ "calendars": out }
}

fn cal_summarize_calendar(item) {
    let id = if item["id"] != () { item["id"] } else { "" };
    let summary = if item["summary"] != () { item["summary"] } else { "" };
    let tz = if item["timeZone"] != () { item["timeZone"] } else { "" };
    let primary = item["primary"] == true;
    let access = if item["accessRole"] != () { item["accessRole"] } else { "" };
    #{
        "id": id,
        "summary": summary,
        "time_zone": tz,
        "primary": primary,
        "access_role": access,
    }
}

fn cal_list_events_impl(token, args) {
    let calendar_id = cal_pick_calendar_id(args);
    let now = now();
    let time_min = if args["time_min"] != () && args["time_min"] != "" {
        args["time_min"]
    } else {
        unix_to_rfc3339(now)
    };
    let time_max = if args["time_max"] != () && args["time_max"] != "" {
        args["time_max"]
    } else {
        unix_to_rfc3339(now + 7 * 24 * 3600)
    };
    let max_results = if args["max_results"] != () {
        let n = args["max_results"];
        if n > 100 { 100 } else if n < 1 { 1 } else { n }
    } else {
        25
    };

    let url = "https://www.googleapis.com/calendar/v3/calendars/"
        + url_encode(calendar_id) + "/events";

    let query = #{
        "timeMin": time_min,
        "timeMax": time_max,
        "maxResults": "" + max_results,
        "singleEvents": "true",
        "orderBy": "startTime",
    };
    if args["query"] != () && args["query"] != "" {
        query["q"] = args["query"];
    }

    let resp = http_get(url, query, token);
    if resp == () || resp["items"] == () {
        return #{
            "events": [],
            "calendar_id": calendar_id,
            "time_min": time_min,
            "time_max": time_max,
        };
    }
    let out = [];
    for ev in resp["items"] {
        out.push(cal_summarize_event(ev));
    }
    let result = #{
        "events": out,
        "calendar_id": calendar_id,
        "time_min": time_min,
        "time_max": time_max,
    };
    if resp["nextPageToken"] != () {
        result["next_page_token"] = resp["nextPageToken"];
    }
    result
}

fn cal_check_availability_impl(token, args) {
    let time_min = if args["time_min"] != () { args["time_min"] } else { "" };
    let time_max = if args["time_max"] != () { args["time_max"] } else { "" };
    if time_min == "" || time_max == "" {
        throw "calendar.check_availability requires both `time_min` and `time_max`";
    }
    let ids_in = if args["calendar_ids"] != () && type_of(args["calendar_ids"]) == "array" {
        args["calendar_ids"]
    } else {
        ["primary"]
    };
    let items = [];
    for id in ids_in {
        items.push(#{ "id": "" + id });
    }
    let body = #{
        "timeMin": time_min,
        "timeMax": time_max,
        "items": items,
    };
    let resp = http_post(
        "https://www.googleapis.com/calendar/v3/freeBusy",
        body,
        token,
    );
    if resp == () { #{} } else { resp }
}

fn cal_get_event_impl(token, args) {
    let calendar_id = cal_pick_calendar_id(args);
    let event_id = if args["event_id"] != () { args["event_id"] } else { "" };
    if event_id == "" {
        throw "calendar.get_event requires `event_id`";
    }
    let url = "https://www.googleapis.com/calendar/v3/calendars/"
        + url_encode(calendar_id) + "/events/" + url_encode(event_id);
    let resp = http_get(url, #{}, token);
    if resp == () {
        throw "calendar.get_event: empty response from Google";
    }
    let normalised = cal_summarize_event(resp);
    normalised["calendar_id"] = calendar_id;
    normalised
}

fn cal_create_event_impl(token, args) {
    let calendar_id = cal_pick_calendar_id(args);
    let summary = if args["summary"] != () { args["summary"] } else { "" };
    let start = if args["start"] != () { args["start"] } else { "" };
    let end = if args["end"] != () { args["end"] } else { "" };
    if summary == "" || start == "" || end == "" {
        throw "calendar.create_event requires `summary`, `start`, `end`";
    }
    let body = cal_build_event_body(args, true);
    let attendees_present = body["attendees"] != ();

    let url = "https://www.googleapis.com/calendar/v3/calendars/"
        + url_encode(calendar_id) + "/events";
    if attendees_present {
        url += "?sendNotifications=true";
    }
    let resp = http_post(url, body, token);
    if resp == () {
        throw "calendar.create_event: empty response from Google";
    }
    let normalised = cal_summarize_event(resp);
    normalised["calendar_id"] = calendar_id;
    normalised
}

fn cal_update_event_impl(token, args) {
    let calendar_id = cal_pick_calendar_id(args);
    let event_id = if args["event_id"] != () { args["event_id"] } else { "" };
    if event_id == "" {
        throw "calendar.update_event requires `event_id`";
    }
    let body = cal_build_event_body(args, false);
    let attendees_present = body["attendees"] != ();

    let url = "https://www.googleapis.com/calendar/v3/calendars/"
        + url_encode(calendar_id) + "/events/" + url_encode(event_id);
    if attendees_present {
        url += "?sendNotifications=true";
    }
    let resp = http_patch(url, body, token);
    if resp == () {
        throw "calendar.update_event: empty response from Google";
    }
    let normalised = cal_summarize_event(resp);
    normalised["calendar_id"] = calendar_id;
    normalised
}

fn cal_delete_event_impl(token, args) {
    let calendar_id = cal_pick_calendar_id(args);
    let event_id = if args["event_id"] != () { args["event_id"] } else { "" };
    if event_id == "" {
        throw "calendar.delete_event requires `event_id`";
    }
    let url = "https://www.googleapis.com/calendar/v3/calendars/"
        + url_encode(calendar_id) + "/events/" + url_encode(event_id);
    http_delete(url, #{}, token);
    #{ "deleted": true, "calendar_id": calendar_id, "event_id": event_id }
}

fn cal_pick_calendar_id(args) {
    if args["calendar_id"] != () && args["calendar_id"] != "" {
        args["calendar_id"]
    } else {
        "primary"
    }
}

fn cal_build_event_body(args, require_all_times) {
    let body = #{};
    if args["summary"] != () { body["summary"] = args["summary"]; }
    if args["description"] != () { body["description"] = args["description"]; }
    if args["location"] != () { body["location"] = args["location"]; }
    if require_all_times || args["start"] != () {
        body["start"] = #{ "dateTime": args["start"] };
    }
    if require_all_times || args["end"] != () {
        body["end"] = #{ "dateTime": args["end"] };
    }
    if args["attendees"] != () && type_of(args["attendees"]) == "array" {
        let arr = [];
        for email in args["attendees"] {
            arr.push(#{ "email": "" + email });
        }
        body["attendees"] = arr;
    }
    body
}

fn cal_summarize_event(ev) {
    let summary = if ev["summary"] != () { ev["summary"] } else { "(no title)" };
    let location = if ev["location"] != () { ev["location"] } else { () };
    let description = if ev["description"] != () { ev["description"] } else { () };
    let html_link = if ev["htmlLink"] != () { ev["htmlLink"] } else { () };
    let id = if ev["id"] != () { ev["id"] } else { "" };
    let status = if ev["status"] != () { ev["status"] } else { "confirmed" };

    let start_block = ev["start"];
    let start = if start_block != () {
        cal_normalise_when(start_block)
    } else {
        ()
    };
    let end_block = ev["end"];
    let end = if end_block != () {
        cal_normalise_when(end_block)
    } else {
        ()
    };

    let attendees = [];
    if ev["attendees"] != () {
        for a in ev["attendees"] {
            let email = if a["email"] != () { a["email"] } else { () };
            let display = if a["displayName"] != () { a["displayName"] } else { () };
            let response = if a["responseStatus"] != () { a["responseStatus"] } else { "needsAction" };
            attendees.push(#{
                "email": email,
                "display_name": display,
                "response_status": response,
            });
        }
    }

    #{
        "id": id,
        "summary": summary,
        "location": location,
        "description": description,
        "html_link": html_link,
        "status": status,
        "start": start,
        "end": end,
        "attendees": attendees,
    }
}

fn cal_normalise_when(block) {
    let dt = if block["dateTime"] != () { block["dateTime"] } else { () };
    let date = if block["date"] != () { block["date"] } else { () };
    let tz = if block["timeZone"] != () { block["timeZone"] } else { () };
    if dt != () {
        #{ "kind": "timed", "iso": dt, "time_zone": tz }
    } else if date != () {
        #{ "kind": "all_day", "date": date, "time_zone": tz }
    } else {
        #{ "kind": "unknown" }
    }
}

// ===========================================================================
// Contacts — ported from plugins/google-contacts/main.rhai.
// ===========================================================================

fn contacts_fetch_connections(token) {
    let resp = http_get_cached(
        "https://people.googleapis.com/v1/people/me/connections",
        #{ "personFields": "names,emailAddresses,phoneNumbers", "pageSize": "1000" },
        token,
        300,
    );
    if resp == () || resp["connections"] == () {
        return ();
    }
    resp["connections"]
}

fn contacts_person_matches(person, transport, target) {
    if transport == "email" {
        let emails = person["emailAddresses"];
        if emails == () { return false; }
        for e in emails {
            if e["value"] == () { continue; }
            if lower(trim(e["value"])) == target {
                return true;
            }
        }
        return false;
    }
    let phones = person["phoneNumbers"];
    if phones == () { return false; }
    for p in phones {
        if p["value"] == () { continue; }
        if digits_only(p["value"]) == target {
            return true;
        }
    }
    false
}

fn contacts_build_match(person) {
    let display = "";
    if person["names"] != () && person["names"].len() > 0 {
        let n = person["names"][0];
        if n["displayName"] != () {
            display = n["displayName"];
        }
    }
    let resource = if person["resourceName"] != () {
        person["resourceName"]
    } else {
        "anon-" + display
    };
    #{
        "stable_principal_id": "pri_google_" + hash(resource),
        "confidence": 0.95,
        "trust_hint": "Contact",
        "display_name": display,
        "tags": ["google_apps"],
        "resolved_by": "google-apps",
    }
}

fn contacts_summarize_person(person) {
    let display = ();
    if person["names"] != () && person["names"].len() > 0 {
        let n = person["names"][0];
        if n["displayName"] != () {
            display = n["displayName"];
        }
    }
    let emails = [];
    if person["emailAddresses"] != () {
        for e in person["emailAddresses"] {
            if e["value"] != () { emails.push(e["value"]); }
        }
    }
    let phones = [];
    if person["phoneNumbers"] != () {
        for p in person["phoneNumbers"] {
            if p["value"] != () { phones.push(p["value"]); }
        }
    }
    let resource = if person["resourceName"] != () {
        person["resourceName"]
    } else {
        "anon-" + display
    };
    #{
        "principal_id": "pri_google_" + hash(resource),
        "display_name": display,
        "emails": emails,
        "phones": phones,
    }
}

fn contacts_list_impl(token, args) {
    let limit = if args.len() > 0 && args["limit"] != () {
        args["limit"]
    } else {
        200
    };
    let connections = contacts_fetch_connections(token);
    if connections == () {
        return #{ "contacts": [], "total": 0, "limit": limit };
    }
    let total = connections.len();
    let out = [];
    let n = if total < limit { total } else { limit };
    for i in 0..n {
        out.push(contacts_summarize_person(connections[i]));
    }
    #{ "contacts": out, "total": total, "limit": limit }
}

// ===========================================================================
// Tasks
// ===========================================================================

fn tasks_pick_tasklist_id(args) {
    if args["tasklist_id"] != () && args["tasklist_id"] != "" {
        args["tasklist_id"]
    } else {
        "@default"
    }
}

fn tasks_list_lists_impl(token) {
    let resp = http_get(
        "https://tasks.googleapis.com/tasks/v1/users/@me/lists",
        #{},
        token,
    );
    if resp == () || resp["items"] == () {
        return #{ "task_lists": [] };
    }
    let out = [];
    for item in resp["items"] {
        out.push(#{
            "id": if item["id"] != () { item["id"] } else { "" },
            "title": if item["title"] != () { item["title"] } else { "" },
        });
    }
    #{ "task_lists": out }
}

fn tasks_summarize_task(t) {
    #{
        "id": if t["id"] != () { t["id"] } else { "" },
        "title": if t["title"] != () { t["title"] } else { "" },
        "notes": if t["notes"] != () { t["notes"] } else { () },
        "due": if t["due"] != () { t["due"] } else { () },
        "status": if t["status"] != () { t["status"] } else { "needsAction" },
        "completed": if t["completed"] != () { t["completed"] } else { () },
        "position": if t["position"] != () { t["position"] } else { () },
    }
}

fn tasks_list_tasks_impl(token, args) {
    let tasklist = tasks_pick_tasklist_id(args);
    let show_completed = if args["show_completed"] != () { args["show_completed"] == true } else { false };
    let max_results = if args["max_results"] != () { args["max_results"] } else { 50 };
    let query = #{
        "showCompleted": if show_completed { "true" } else { "false" },
        "maxResults": "" + max_results,
    };
    let url = "https://tasks.googleapis.com/tasks/v1/lists/"
        + url_encode(tasklist) + "/tasks";
    let resp = http_get(url, query, token);
    if resp == () || resp["items"] == () {
        return #{ "tasks": [], "tasklist_id": tasklist };
    }
    let out = [];
    for t in resp["items"] {
        out.push(tasks_summarize_task(t));
    }
    #{ "tasks": out, "tasklist_id": tasklist }
}

fn tasks_create_task_impl(token, args) {
    let tasklist = tasks_pick_tasklist_id(args);
    let title = if args["title"] != () { args["title"] } else { "" };
    if title == "" {
        throw "tasks.create_task requires `title`";
    }
    let body = #{ "title": title };
    if args["notes"] != () { body["notes"] = args["notes"]; }
    if args["due"] != () && args["due"] != "" { body["due"] = args["due"]; }
    let url = "https://tasks.googleapis.com/tasks/v1/lists/"
        + url_encode(tasklist) + "/tasks";
    let resp = http_post(url, body, token);
    if resp == () {
        throw "tasks.create_task: empty response from Google";
    }
    let out = tasks_summarize_task(resp);
    out["tasklist_id"] = tasklist;
    out
}

fn tasks_update_task_impl(token, args) {
    let tasklist = tasks_pick_tasklist_id(args);
    let task_id = if args["task_id"] != () { args["task_id"] } else { "" };
    if task_id == "" {
        throw "tasks.update_task requires `task_id`";
    }
    let body = #{};
    if args["title"] != () { body["title"] = args["title"]; }
    if args["notes"] != () { body["notes"] = args["notes"]; }
    if args["due"] != () { body["due"] = args["due"]; }
    if args["completed"] != () {
        if args["completed"] == true {
            body["status"] = "completed";
            body["completed"] = unix_to_rfc3339(now());
        } else {
            body["status"] = "needsAction";
            // Clearing completed is done by setting it to null —
            // Google accepts the `completed` field being omitted on
            // update with status=needsAction.
        }
    }
    let url = "https://tasks.googleapis.com/tasks/v1/lists/"
        + url_encode(tasklist) + "/tasks/" + url_encode(task_id);
    let resp = http_patch(url, body, token);
    if resp == () {
        throw "tasks.update_task: empty response from Google";
    }
    let out = tasks_summarize_task(resp);
    out["tasklist_id"] = tasklist;
    out
}

fn tasks_complete_task_impl(token, args) {
    let merged = #{};
    if args["tasklist_id"] != () { merged["tasklist_id"] = args["tasklist_id"]; }
    if args["task_id"] != () { merged["task_id"] = args["task_id"]; }
    merged["completed"] = true;
    tasks_update_task_impl(token, merged)
}

fn tasks_delete_task_impl(token, args) {
    let tasklist = tasks_pick_tasklist_id(args);
    let task_id = if args["task_id"] != () { args["task_id"] } else { "" };
    if task_id == "" {
        throw "tasks.delete_task requires `task_id`";
    }
    let url = "https://tasks.googleapis.com/tasks/v1/lists/"
        + url_encode(tasklist) + "/tasks/" + url_encode(task_id);
    http_delete(url, #{}, token);
    #{ "deleted": true, "tasklist_id": tasklist, "task_id": task_id }
}

// ===========================================================================
// Drive
// ===========================================================================

fn drive_summarize_file(f) {
    #{
        "id": if f["id"] != () { f["id"] } else { "" },
        "name": if f["name"] != () { f["name"] } else { "" },
        "mime_type": if f["mimeType"] != () { f["mimeType"] } else { "" },
        "modified_time": if f["modifiedTime"] != () { f["modifiedTime"] } else { () },
        "web_view_link": if f["webViewLink"] != () { f["webViewLink"] } else { () },
        "parents": if f["parents"] != () && type_of(f["parents"]) == "array" { f["parents"] } else { [] },
    }
}

fn drive_search_impl(token, args) {
    let query = if args["query"] != () { args["query"] } else { "" };
    if query == "" {
        throw "drive.search requires `query`";
    }
    let max_results = if args["max_results"] != () { args["max_results"] } else { 25 };
    // Drive's `q` syntax is its own thing — escape single quotes by
    // backslash and build a `name contains 'xxx'` query.
    let escaped = query.replace("'", "\\'");
    let q_built = "name contains '" + escaped + "' and trashed = false";
    if args["mime_type"] != () && args["mime_type"] != "" {
        q_built += " and mimeType = '" + args["mime_type"] + "'";
    }
    let params = #{
        "q": q_built,
        "pageSize": "" + max_results,
        "fields": "files(id,name,mimeType,modifiedTime,webViewLink,parents),nextPageToken",
    };
    let resp = http_get(
        "https://www.googleapis.com/drive/v3/files",
        params,
        token,
    );
    if resp == () || resp["files"] == () {
        return #{ "files": [] };
    }
    let out = [];
    for f in resp["files"] {
        out.push(drive_summarize_file(f));
    }
    let result = #{ "files": out };
    if resp["nextPageToken"] != () {
        result["next_page_token"] = resp["nextPageToken"];
    }
    result
}

fn drive_get_file_metadata_impl(token, args) {
    let file_id = if args["file_id"] != () { args["file_id"] } else { "" };
    if file_id == "" {
        throw "drive.get_file_metadata requires `file_id`";
    }
    let url = "https://www.googleapis.com/drive/v3/files/" + url_encode(file_id);
    let resp = http_get(url, #{ "fields": "*" }, token);
    if resp == () {
        throw "drive.get_file_metadata: empty response from Google";
    }
    drive_summarize_file(resp)
}

fn drive_list_folder_impl(token, args) {
    let folder_id = if args["folder_id"] != () { args["folder_id"] } else { "" };
    if folder_id == "" {
        throw "drive.list_folder requires `folder_id`";
    }
    let max_results = if args["max_results"] != () { args["max_results"] } else { 25 };
    let escaped = folder_id.replace("'", "\\'");
    let q_built = "'" + escaped + "' in parents and trashed = false";
    let params = #{
        "q": q_built,
        "pageSize": "" + max_results,
        "fields": "files(id,name,mimeType,modifiedTime,webViewLink,parents),nextPageToken",
    };
    let resp = http_get(
        "https://www.googleapis.com/drive/v3/files",
        params,
        token,
    );
    if resp == () || resp["files"] == () {
        return #{ "files": [], "folder_id": folder_id };
    }
    let out = [];
    for f in resp["files"] {
        out.push(drive_summarize_file(f));
    }
    #{ "files": out, "folder_id": folder_id }
}

fn drive_read_file_impl(token, args) {
    let file_id = if args["file_id"] != () { args["file_id"] } else { "" };
    if file_id == "" {
        throw "drive.read_file requires `file_id`";
    }
    let max_bytes = if args["max_bytes"] != () {
        let n = args["max_bytes"];
        if n > 10485760 { 10485760 } else if n < 1 { 1 } else { n }
    } else {
        1048576
    };
    // Lookup metadata first to know whether to use export or alt=media.
    let meta_url = "https://www.googleapis.com/drive/v3/files/" + url_encode(file_id);
    let meta = http_get(meta_url, #{ "fields": "id,name,mimeType,size" }, token);
    if meta == () {
        throw "drive.read_file: file not found";
    }
    let mime = if meta["mimeType"] != () { meta["mimeType"] } else { "" };
    let is_google_doc = mime.starts_with("application/vnd.google-apps");
    let url = if is_google_doc {
        "https://www.googleapis.com/drive/v3/files/" + url_encode(file_id)
            + "/export?mimeType=text/plain"
    } else {
        "https://www.googleapis.com/drive/v3/files/" + url_encode(file_id) + "?alt=media"
    };
    let raw = http_get(url, #{}, token);
    // The host's http_get auto-decodes JSON; for Drive media the
    // response is raw bytes / text. Plugins receive the raw body
    // when the Content-Type isn't application/json — which Drive
    // honours via the alt=media query. We pass through as a string
    // when decodable.
    if raw == () {
        return #{
            "file_id": file_id,
            "name": if meta["name"] != () { meta["name"] } else { "" },
            "mime_type": mime,
            "content": "",
        };
    }
    let content_str = "" + raw;
    let size = content_str.len();
    if size > max_bytes {
        content_str = content_str.sub_string(0, max_bytes);
    }
    #{
        "file_id": file_id,
        "name": if meta["name"] != () { meta["name"] } else { "" },
        "mime_type": mime,
        "content": content_str,
        "truncated": size > max_bytes,
    }
}

fn drive_create_file_impl(token, args) {
    let name = if args["name"] != () { args["name"] } else { "" };
    if name == "" {
        throw "drive.create_file requires `name`";
    }
    let content = if args["content"] != () { args["content"] } else { "" };
    let mime = if args["mime_type"] != () && args["mime_type"] != "" {
        args["mime_type"]
    } else {
        "text/plain"
    };

    // Drive's multipart upload uses a synthesised boundary. We build
    // it with mime/related and pass through http_post with a custom
    // Content-Type header. The host's http_post serialises maps to
    // JSON unless given a string body — pass the raw multipart text
    // as a string.
    let boundary = "execlaw-drive-boundary-" + hash("" + now() + name);
    let metadata = #{ "name": name, "mimeType": mime };
    if args["parent_folder_id"] != () && args["parent_folder_id"] != "" {
        metadata["parents"] = [args["parent_folder_id"]];
    }
    let metadata_json = to_json_string(metadata);
    let body = "--" + boundary + "\r\n"
        + "Content-Type: application/json; charset=UTF-8\r\n\r\n"
        + metadata_json + "\r\n"
        + "--" + boundary + "\r\n"
        + "Content-Type: " + mime + "\r\n\r\n"
        + content + "\r\n"
        + "--" + boundary + "--";
    let headers = #{
        "Content-Type": "multipart/related; boundary=" + boundary,
    };
    let url = "https://www.googleapis.com/upload/drive/v3/files?uploadType=multipart&fields=id,name,mimeType,webViewLink,parents";
    let resp = http_post(url, body, token, headers);
    if resp == () {
        throw "drive.create_file: empty response from Google";
    }
    drive_summarize_file(resp)
}

fn drive_share_impl(token, args) {
    let file_id = if args["file_id"] != () { args["file_id"] } else { "" };
    let email = if args["email"] != () { args["email"] } else { "" };
    if file_id == "" || email == "" {
        throw "drive.share requires `file_id` and `email`";
    }
    let role = if args["role"] != () && args["role"] != "" {
        args["role"]
    } else {
        "reader"
    };
    if role != "reader" && role != "writer" && role != "commenter" {
        throw "drive.share role must be 'reader', 'writer', or 'commenter'";
    }
    let body = #{
        "type": "user",
        "role": role,
        "emailAddress": email,
    };
    let url = "https://www.googleapis.com/drive/v3/files/"
        + url_encode(file_id) + "/permissions?sendNotificationEmail=false";
    let resp = http_post(url, body, token);
    if resp == () {
        throw "drive.share: empty response from Google";
    }
    #{
        "shared": true,
        "file_id": file_id,
        "email": email,
        "role": role,
        "permission_id": if resp["id"] != () { resp["id"] } else { "" },
    }
}
