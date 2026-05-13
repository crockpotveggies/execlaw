// google-calendar — Google Calendar API tools.
//
// Seven host-dispatched entry points:
//
//   tool_call("calendar.list_calendars", #{}, oauth)
//     -> #{ "calendars": [...] }
//
//   tool_call("calendar.list_events", #{
//       "calendar_id"?: "primary" | "<calendar_id>",
//       "time_min"?:  "2026-05-01T00:00:00Z",
//       "time_max"?:  "2026-06-01T00:00:00Z",
//       "max_results"?: 25,
//       "query"?:    "free-text search",
//     }, oauth)
//     -> #{ "events": [...], "calendar_id": "...", "next_page_token"?: "..." }
//
//   tool_call("calendar.check_availability", #{
//       "time_min": "...", "time_max": "...",
//       "calendar_ids"?: ["primary", "..."],
//     }, oauth)
//     -> Google freeBusy response (calendars keyed by id with `busy` arrays).
//
//   tool_call("calendar.get_event", #{
//       "calendar_id"?: "primary", "event_id": "...",
//     }, oauth)
//     -> normalised event payload (same shape as list_events rows).
//
//   tool_call("calendar.create_event", #{
//       "summary": "...", "start": "...", "end": "...",
//       "calendar_id"?: "primary", "description"?, "location"?, "attendees"?: ["e@x"],
//     }, oauth)
//     -> normalised event payload.
//
//   tool_call("calendar.update_event", #{
//       "event_id": "...",  // required
//       "calendar_id"?: "primary",
//       "summary"?, "start"?, "end"?, "description"?, "location"?, "attendees"?,
//     }, oauth)
//     -> normalised event payload (post-patch).
//
//   tool_call("calendar.delete_event", #{
//       "calendar_id"?: "primary", "event_id": "...",
//     }, oauth)
//     -> #{ "deleted": true }
//
// `oauth` is a map of {account_name: access_token} the host
// already injected from state_oauth_tokens. We never see the
// refresh_token or client_secret — those stay on the server,
// rotated by the OAuth sweeper.
//
// Caching:
//   * list_calendars is cached 5 min — calendar metadata changes
//     rarely, the operator can hit "Refresh" if needed.
//   * list_events / check_availability / get_event are NOT cached
//     — events shift quickly during a scheduling conversation and
//     Google's rate limits are generous; freshness > 60 ms savings.
//   * create_event / update_event / delete_event are mutations and
//     are obviously never cached.
//
// Mutations send notifications when attendees are present.
// Google's `sendNotifications=true` query param is appended to the
// create/update/delete URLs only when there's at least one
// attendee, mirroring selfhosted-claw's calendar runtime.
//
// Note on Rhai scope: top-level `const` declarations aren't
// visible inside `fn` bodies, so the URL + cache TTL are inlined
// inside each fetch helper. Same gotcha as the google-contacts
// plugin documents.

// ---------------------------------------------------------------------------
// Lifecycle (2026-05-13)
//
// google-calendar is HTTP-only against Google's REST APIs. No
// WS subscription, no sidecar, no background task. The only
// state is the OAuth refresh + access tokens in
// `state_oauth_tokens`, which factory reset's DB-wide truncate
// wipes.
//
// Ideally on_disable would call Google's revocation endpoint
// (`POST https://oauth2.googleapis.com/revoke?token=...`) so the
// refresh token is invalidated on Google's side too — leaking
// only the access tokens (1h TTL) instead of the long-lived
// refresh token. That requires the host to surface an
// `oauth_revoke` Rhai binding which doesn't exist yet (tracked
// separately). For now we just log the lifecycle hook firing;
// the local token rows are still wiped by the DB truncate.

fn on_disable() {
    log_info("google-calendar: on_disable — no remote token revocation (oauth_revoke binding not yet available); local tokens will be wiped with the DB");
    ()
}

// ---------------------------------------------------------------------------
// Public dispatch surface (the host calls this by name).

fn tool_call(name, args, oauth) {
    let token = oauth["controller"];
    if token == () {
        throw "Google account not connected — visit Settings → Plugins → google-calendar to configure";
    }
    if name == "calendar.list_calendars" {
        return list_calendars_impl(token);
    }
    if name == "calendar.list_events" {
        return list_events_impl(token, args);
    }
    if name == "calendar.check_availability" {
        return check_availability_impl(token, args);
    }
    if name == "calendar.get_event" {
        return get_event_impl(token, args);
    }
    if name == "calendar.create_event" {
        return create_event_impl(token, args);
    }
    if name == "calendar.update_event" {
        return update_event_impl(token, args);
    }
    if name == "calendar.delete_event" {
        return delete_event_impl(token, args);
    }
    throw "unknown tool '" + name + "'";
}

// ---------------------------------------------------------------------------
// list_calendars — small, cacheable, rare-change.

fn list_calendars_impl(token) {
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
        out.push(summarize_calendar(item));
    }
    #{ "calendars": out }
}

fn summarize_calendar(item) {
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

// ---------------------------------------------------------------------------
// list_events — windowed by (time_min, time_max), optional `query`.
// Defaults to the next 7 days from "now" if no window given.

fn list_events_impl(token, args) {
    let calendar_id = pick_calendar_id(args);
    let now = now();
    let time_min = if args["time_min"] != () && args["time_min"] != "" {
        args["time_min"]
    } else {
        unix_to_rfc3339(now)
    };
    let time_max = if args["time_max"] != () && args["time_max"] != "" {
        args["time_max"]
    } else {
        // Default window: next 7 days.
        unix_to_rfc3339(now + 7 * 24 * 3600)
    };
    // Cap max_results at 100 — Google's hard upper limit per page.
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
    // Optional free-text search — mirrors the `q` param exposed
    // by Google + selfhosted-claw's calendar_list_events.
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
        out.push(summarize_event(ev));
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

// ---------------------------------------------------------------------------
// check_availability — POST /freeBusy with one or more calendar ids.

fn check_availability_impl(token, args) {
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
    // Pass the raw freeBusy response through — the agent reasons
    // over the per-calendar `busy` arrays directly.
    if resp == () { #{} } else { resp }
}

// ---------------------------------------------------------------------------
// get_event — GET single event, normalised through summarize_event.

fn get_event_impl(token, args) {
    let calendar_id = pick_calendar_id(args);
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
    let normalised = summarize_event(resp);
    normalised["calendar_id"] = calendar_id;
    normalised
}

// ---------------------------------------------------------------------------
// create_event — POST a new event. Adds `sendNotifications=true`
// when attendees are present so invitations actually go out.

fn create_event_impl(token, args) {
    let calendar_id = pick_calendar_id(args);
    let summary = if args["summary"] != () { args["summary"] } else { "" };
    let start = if args["start"] != () { args["start"] } else { "" };
    let end = if args["end"] != () { args["end"] } else { "" };
    if summary == "" || start == "" || end == "" {
        throw "calendar.create_event requires `summary`, `start`, `end`";
    }
    let body = build_event_body(args, true);
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
    let normalised = summarize_event(resp);
    normalised["calendar_id"] = calendar_id;
    normalised
}

// ---------------------------------------------------------------------------
// update_event — PATCH partial body. Same notification rule.

fn update_event_impl(token, args) {
    let calendar_id = pick_calendar_id(args);
    let event_id = if args["event_id"] != () { args["event_id"] } else { "" };
    if event_id == "" {
        throw "calendar.update_event requires `event_id`";
    }
    let body = build_event_body(args, false);
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
    let normalised = summarize_event(resp);
    normalised["calendar_id"] = calendar_id;
    normalised
}

// ---------------------------------------------------------------------------
// delete_event — DELETE. 204 No Content is the success path.

fn delete_event_impl(token, args) {
    let calendar_id = pick_calendar_id(args);
    let event_id = if args["event_id"] != () { args["event_id"] } else { "" };
    if event_id == "" {
        throw "calendar.delete_event requires `event_id`";
    }
    let url = "https://www.googleapis.com/calendar/v3/calendars/"
        + url_encode(calendar_id) + "/events/" + url_encode(event_id);
    // http_delete throws on non-2xx; an Ok return means Google
    // accepted the cancellation regardless of response body.
    http_delete(url, #{}, token);
    #{ "deleted": true, "calendar_id": calendar_id, "event_id": event_id }
}

// ---------------------------------------------------------------------------
// Shared helpers.

fn pick_calendar_id(args) {
    if args["calendar_id"] != () && args["calendar_id"] != "" {
        args["calendar_id"]
    } else {
        "primary"
    }
}

// Build the JSON body for create/update. When `require_all_times`
// is true (create), `start` + `end` are unconditionally written;
// when false (update), only the fields the caller actually
// supplied are emitted, since PATCH treats omitted fields as
// "leave unchanged".
fn build_event_body(args, require_all_times) {
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

// Normalise an event into the shape the agent / SPA consumes.
// Google represents start/end as either { dateTime, timeZone }
// (timed event) or { date } (all-day). We surface both forms;
// agent decides what to render.
fn summarize_event(ev) {
    let summary = if ev["summary"] != () { ev["summary"] } else { "(no title)" };
    let location = if ev["location"] != () { ev["location"] } else { () };
    let description = if ev["description"] != () { ev["description"] } else { () };
    let html_link = if ev["htmlLink"] != () { ev["htmlLink"] } else { () };
    let id = if ev["id"] != () { ev["id"] } else { "" };
    let status = if ev["status"] != () { ev["status"] } else { "confirmed" };

    let start_block = ev["start"];
    let start = if start_block != () {
        normalise_when(start_block)
    } else {
        ()
    };
    let end_block = ev["end"];
    let end = if end_block != () {
        normalise_when(end_block)
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

fn normalise_when(block) {
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

// Date formatting + URL encoding live as host-injected primitives
// (`unix_to_rfc3339`, `url_encode`) — date math + percent-encoding
// in pure Rhai is gnarly enough that pushing it down to Rust is
// the right separation. Future plugins reuse the same helpers.
