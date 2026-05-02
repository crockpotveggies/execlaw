// google-calendar — Google Calendar API tools (read-only).
//
// Two host-dispatched entry points:
//
//   tool_call("calendar.list_calendars", #{}, oauth)
//     -> #{ "calendars": [...] }
//
//   tool_call("calendar.list_events", #{
//       "calendar_id"?: "primary" | "<calendar_id>",
//       "time_min"?: "2026-05-01T00:00:00Z",
//       "time_max"?: "2026-06-01T00:00:00Z",
//       "max_results"?: 25,
//     }, oauth)
//     -> #{ "events": [...], "calendar_id": "...", "next_page_token"?: "..." }
//
// `oauth` is a map of {account_name: access_token} the host
// already injected from state_oauth_tokens. We never see the
// refresh_token or client_secret — those stay on the server,
// rotated by the OAuth sweeper.
//
// Caching:
//   * list_calendars is cached 5 min — calendar metadata changes
//     rarely, the operator can hit "Refresh" if needed.
//   * list_events is NOT cached (default) — events are
//     time-windowed and Google's rate limits are generous; we'd
//     rather pay a fresh call than serve stale results during a
//     conversation about the day's schedule. If the cost shows
//     up in production we can add a 60 s cache keyed on the
//     full arg map.
//
// Note on Rhai scope: top-level `const` declarations aren't
// visible inside `fn` bodies, so the URL + cache TTL are inlined
// inside each fetch helper. Same gotcha as the google-contacts
// plugin documents.

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
// list_events — windowed by (time_min, time_max).
// Defaults to the next 7 days from "now" if no window given.

fn list_events_impl(token, args) {
    let calendar_id = if args["calendar_id"] != () && args["calendar_id"] != "" {
        args["calendar_id"]
    } else {
        "primary"
    };
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

    // calendar_id has to be URL-path-encoded (e.g. user@gmail.com
    // contains '@'). The host's http_get takes the URL verbatim,
    // so we hand it a pre-encoded path. Common case `primary`
    // doesn't need encoding.
    let url = "https://www.googleapis.com/calendar/v3/calendars/"
        + url_encode(calendar_id) + "/events";

    let resp = http_get(
        url,
        #{
            "timeMin": time_min,
            "timeMax": time_max,
            "maxResults": "" + max_results,
            "singleEvents": "true",
            "orderBy": "startTime",
        },
        token,
    );
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
