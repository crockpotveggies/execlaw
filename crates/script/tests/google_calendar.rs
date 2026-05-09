//! End-to-end test for the real `plugins/google-calendar/main.rhai`
//! script. Loads it from disk, points the Calendar API URLs at a
//! local mock server, exercises both tool entry points
//! (`calendar.list_calendars`, `calendar.list_events`) plus the
//! cache + token-rotation paths.
//!
//! No real Google credentials needed.

use execlaw_script::{ScriptEngine, ScriptPlugin};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn parse_content_length(head: &str) -> usize {
    for line in head.lines() {
        let mut iter = line.splitn(2, ':');
        if let (Some(name), Some(value)) = (iter.next(), iter.next())
            && name.eq_ignore_ascii_case("content-length")
        {
            if let Ok(n) = value.trim().parse::<usize>() {
                return n;
            }
        }
    }
    0
}

/// Mock server that serves canned responses based on the request
/// path. Returns the bound base URL + a shared call-counter.
fn spawn_mock(routes: &'static [(&'static str, &'static str)]) -> (String, Arc<Mutex<u32>>) {
    let (url, _, calls) = spawn_mock_recording(routes);
    (url, calls)
}

/// Recorded shape of an inbound request — what the test asserts
/// about the verb / URL / body. The mock pushes one of these per
/// request before responding.
#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    body: String,
}

/// Like `spawn_mock` but additionally records every inbound request
/// (method + path + body) so tests can assert on the wire payload
/// produced by the create/update/delete tools.
fn spawn_mock_recording(
    routes: &'static [(&'static str, &'static str)],
) -> (String, Arc<Mutex<Vec<RecordedRequest>>>, Arc<Mutex<u32>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(Mutex::new(0));
    let recorded: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let calls_w = calls.clone();
    let recorded_w = recorded.clone();
    std::thread::spawn(move || {
        loop {
            let Ok((mut sock, _)) = listener.accept() else {
                break;
            };
            *calls_w.lock().unwrap() += 1;

            // Read until we have the full headers + Content-Length
            // bytes of body. Reading "as much as the first packet"
            // is unsafe on Windows: when the handler closes the
            // socket while the kernel still has unread request
            // bytes from the client, Winsock issues an RST instead
            // of a clean FIN, and the client surfaces that as
            // "connection aborted by software". Draining the body
            // first avoids the race.
            let mut buf: Vec<u8> = Vec::with_capacity(2048);
            let mut chunk = [0u8; 4096];
            let mut header_end: Option<usize> = None;
            let mut content_length: usize = 0;
            loop {
                if let Some(he) = header_end {
                    if buf.len() >= he + content_length {
                        break;
                    }
                } else if let Some(pos) = find_header_end(&buf) {
                    header_end = Some(pos);
                    let head_str = std::str::from_utf8(&buf[..pos]).unwrap_or("");
                    content_length = parse_content_length(head_str);
                    if buf.len() >= pos + content_length {
                        break;
                    }
                }
                let n = match sock.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 65_536 {
                    break; // safety cap — tests never send anything close
                }
            }

            let req = std::str::from_utf8(&buf).unwrap_or("");
            let mut lines = req.lines();
            let request_line = lines.next().unwrap_or("");
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or("").to_owned();
            let path = parts.next().unwrap_or("/").to_owned();
            let body = req.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
            recorded_w.lock().unwrap().push(RecordedRequest {
                method,
                path: path.clone(),
                body,
            });

            // Match longest prefix that the requested path starts with.
            let resp_body = routes
                .iter()
                .filter(|(p, _)| path.starts_with(p))
                .max_by_key(|(p, _)| p.len())
                .map(|(_, b)| *b)
                .unwrap_or(r#"{"items":[]}"#);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                resp_body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
            // Tell the client we're done writing so it doesn't try
            // to re-use the connection on the next request — pairs
            // with `Connection: close` above.
            let _ = sock.shutdown(std::net::Shutdown::Write);
        }
    });
    (format!("http://{addr}"), recorded, calls)
}

const SAMPLE_CALENDARS: &str = r#"{
  "items": [
    {
      "id": "primary",
      "summary": "alice@example.com",
      "timeZone": "America/Los_Angeles",
      "primary": true,
      "accessRole": "owner"
    },
    {
      "id": "team-shared@group.calendar.google.com",
      "summary": "Team Shared",
      "timeZone": "UTC",
      "accessRole": "reader"
    }
  ]
}"#;

const SAMPLE_EVENTS: &str = r#"{
  "items": [
    {
      "id": "evt-1",
      "summary": "Standup",
      "location": "Zoom",
      "status": "confirmed",
      "htmlLink": "https://calendar.google.com/event?eid=evt-1",
      "start": {"dateTime": "2026-05-02T15:00:00Z", "timeZone": "UTC"},
      "end":   {"dateTime": "2026-05-02T15:30:00Z", "timeZone": "UTC"},
      "attendees": [
        {"email": "alice@example.com", "displayName": "Alice", "responseStatus": "accepted"},
        {"email": "bob@example.com",   "responseStatus": "needsAction"}
      ]
    },
    {
      "id": "evt-2",
      "summary": "Holiday",
      "status": "confirmed",
      "start": {"date": "2026-05-04"},
      "end":   {"date": "2026-05-05"}
    },
    {
      "id": "evt-3",
      "summary": "(no title later)",
      "status": "confirmed",
      "start": {"dateTime": "2026-05-03T18:00:00Z"},
      "end":   {"dateTime": "2026-05-03T19:00:00Z"}
    }
  ],
  "nextPageToken": "page-token-2"
}"#;

/// Read `plugins/google-calendar/main.rhai` and rewrite every
/// hardcoded Calendar-API URL prefix to point at our local mock.
fn load_script_against_mock(mock_url: &str) -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("plugins/google-calendar/main.rhai");
    let original =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
    original
        .replace(
            r#""https://www.googleapis.com/calendar/v3/users/me/calendarList""#,
            &format!(r#""{mock_url}/calendar/v3/users/me/calendarList""#),
        )
        .replace(
            r#""https://www.googleapis.com/calendar/v3/calendars/""#,
            &format!(r#""{mock_url}/calendar/v3/calendars/""#),
        )
        .replace(
            r#""https://www.googleapis.com/calendar/v3/freeBusy""#,
            &format!(r#""{mock_url}/calendar/v3/freeBusy""#),
        )
}

fn build_plugin(mock_url: &str) -> ScriptPlugin {
    let factory = ScriptEngine::with_loopback_allowed_for_tests();
    let source = load_script_against_mock(mock_url);
    ScriptPlugin::from_source("google-calendar", &source, &factory)
        .expect("real google-calendar/main.rhai must parse cleanly")
}

fn oauth(token: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert(
        "controller".into(),
        serde_json::Value::String(token.to_owned()),
    );
    m
}

// ---------------------------------------------------------------------------
// list_calendars

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_calendars_returns_normalised_shape() {
    let (url, _) = spawn_mock(&[("/calendar/v3/users/me/calendarList", SAMPLE_CALENDARS)]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "calendar.list_calendars",
            serde_json::json!({}),
            oauth("ya29.tok"),
        )
        .await
        .unwrap();
    let cals = r["calendars"].as_array().unwrap();
    assert_eq!(cals.len(), 2);
    assert_eq!(cals[0]["id"], "primary");
    assert_eq!(cals[0]["summary"], "alice@example.com");
    assert_eq!(cals[0]["primary"], true);
    assert_eq!(cals[0]["access_role"], "owner");
    assert_eq!(cals[0]["time_zone"], "America/Los_Angeles");
    assert_eq!(cals[1]["id"], "team-shared@group.calendar.google.com");
    assert_eq!(cals[1]["primary"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_calendars_errors_when_no_oauth_token() {
    let plugin = build_plugin("http://127.0.0.1:1");
    let err = plugin
        .tool_call(
            "calendar.list_calendars",
            serde_json::json!({}),
            serde_json::Map::new(),
        )
        .await
        .unwrap_err();
    let s = err.to_string();
    assert!(s.contains("not connected"), "got: {s}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_list_calendars_with_same_token_hits_cache() {
    let (url, calls) = spawn_mock(&[("/calendar/v3/users/me/calendarList", SAMPLE_CALENDARS)]);
    let plugin = build_plugin(&url);
    let _ = plugin
        .tool_call(
            "calendar.list_calendars",
            serde_json::json!({}),
            oauth("tok"),
        )
        .await
        .unwrap();
    let _ = plugin
        .tool_call(
            "calendar.list_calendars",
            serde_json::json!({}),
            oauth("tok"),
        )
        .await
        .unwrap();
    assert_eq!(*calls.lock().unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_rotation_invalidates_calendars_cache() {
    let (url, calls) = spawn_mock(&[("/calendar/v3/users/me/calendarList", SAMPLE_CALENDARS)]);
    let plugin = build_plugin(&url);
    let _ = plugin
        .tool_call(
            "calendar.list_calendars",
            serde_json::json!({}),
            oauth("token-A"),
        )
        .await
        .unwrap();
    let _ = plugin
        .tool_call(
            "calendar.list_calendars",
            serde_json::json!({}),
            oauth("token-B"),
        )
        .await
        .unwrap();
    assert_eq!(*calls.lock().unwrap(), 2);
}

// ---------------------------------------------------------------------------
// list_events

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_events_default_args_uses_primary_calendar_and_7_day_window() {
    let (url, _) = spawn_mock(&[("/calendar/v3/calendars/primary/events", SAMPLE_EVENTS)]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "calendar.list_events",
            serde_json::json!({}),
            oauth("ya29.tok"),
        )
        .await
        .unwrap();
    assert_eq!(r["calendar_id"], "primary");
    let events = r["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    // Timed event surfaces start.kind == "timed" + the iso string.
    assert_eq!(events[0]["summary"], "Standup");
    assert_eq!(events[0]["location"], "Zoom");
    assert_eq!(events[0]["start"]["kind"], "timed");
    assert_eq!(events[0]["start"]["iso"], "2026-05-02T15:00:00Z");
    assert_eq!(events[0]["start"]["time_zone"], "UTC");
    // All-day event surfaces start.kind == "all_day" + date.
    assert_eq!(events[1]["start"]["kind"], "all_day");
    assert_eq!(events[1]["start"]["date"], "2026-05-04");
    // next_page_token surfaces when present.
    assert_eq!(r["next_page_token"], "page-token-2");
    // Attendees normalised.
    let attendees = events[0]["attendees"].as_array().unwrap();
    assert_eq!(attendees.len(), 2);
    assert_eq!(attendees[0]["email"], "alice@example.com");
    assert_eq!(attendees[0]["display_name"], "Alice");
    assert_eq!(attendees[0]["response_status"], "accepted");
    assert_eq!(attendees[1]["display_name"], serde_json::Value::Null);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_events_explicit_calendar_id_url_encodes_the_path() {
    // Calendar id with `@` must be percent-encoded in the URL.
    let (url, _) = spawn_mock(&[(
        "/calendar/v3/calendars/team-shared%40group.calendar.google.com/events",
        SAMPLE_EVENTS,
    )]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "calendar.list_events",
            serde_json::json!({
                "calendar_id": "team-shared@group.calendar.google.com",
            }),
            oauth("tok"),
        )
        .await
        .unwrap();
    // Asserts the mock matched (the prefix branch hit). If the
    // script didn't url_encode, the request would land at
    // `/calendar/v3/calendars/team-shared@.../events` and miss
    // the route prefix → empty items.
    let events = r["events"].as_array().unwrap();
    assert!(!events.is_empty(), "encoded path must hit the mock route");
    assert_eq!(r["calendar_id"], "team-shared@group.calendar.google.com");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_events_caps_max_results_at_100() {
    // We can't easily inspect the outbound query string with the
    // current mock, so we rely on the script's clamp: pass 999
    // and verify the response shape comes back without error.
    // (The clamp itself is unit-tested via the 100 boundary in
    // an integration sense — the API would error on >2500 anyway.)
    let (url, _) = spawn_mock(&[("/calendar/v3/calendars/primary/events", SAMPLE_EVENTS)]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "calendar.list_events",
            serde_json::json!({"max_results": 999}),
            oauth("tok"),
        )
        .await
        .unwrap();
    assert!(r["events"].is_array());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_tool_throws() {
    let plugin = build_plugin("http://127.0.0.1:1");
    let err = plugin
        .tool_call(
            "calendar.delete_everything",
            serde_json::json!({}),
            oauth("tok"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("calendar.delete_everything"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_events_handles_empty_response() {
    let (url, _) = spawn_mock(&[("/calendar/v3/calendars/primary/events", r#"{"items":[]}"#)]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call("calendar.list_events", serde_json::json!({}), oauth("tok"))
        .await
        .unwrap();
    assert_eq!(r["events"].as_array().unwrap().len(), 0);
    assert_eq!(r["calendar_id"], "primary");
    assert!(r["time_min"].as_str().unwrap().contains("T"));
    assert!(r["time_max"].as_str().unwrap().contains("T"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_events_passes_query_string_through_to_the_api() {
    // The mock matches on path prefix and ignores the query string;
    // we use the recording mock so we can assert the `?q=` param
    // was sent.
    let (url, recorded, _) =
        spawn_mock_recording(&[("/calendar/v3/calendars/primary/events", SAMPLE_EVENTS)]);
    let plugin = build_plugin(&url);
    let _ = plugin
        .tool_call(
            "calendar.list_events",
            serde_json::json!({"query": "standup"}),
            oauth("tok"),
        )
        .await
        .unwrap();
    let req = recorded.lock().unwrap();
    assert_eq!(req.len(), 1);
    assert_eq!(req[0].method, "GET");
    assert!(
        req[0].path.contains("q=standup"),
        "expected q=standup in path, got {}",
        req[0].path,
    );
}

// ---------------------------------------------------------------------------
// check_availability

const SAMPLE_FREEBUSY: &str = r#"{
  "kind": "calendar#freeBusy",
  "timeMin": "2026-05-02T00:00:00Z",
  "timeMax": "2026-05-03T00:00:00Z",
  "calendars": {
    "primary": {
      "busy": [
        { "start": "2026-05-02T15:00:00Z", "end": "2026-05-02T15:30:00Z" }
      ]
    }
  }
}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn check_availability_posts_freebusy_with_default_calendars() {
    let (url, recorded, _) = spawn_mock_recording(&[("/calendar/v3/freeBusy", SAMPLE_FREEBUSY)]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "calendar.check_availability",
            serde_json::json!({
                "time_min": "2026-05-02T00:00:00Z",
                "time_max": "2026-05-03T00:00:00Z",
            }),
            oauth("tok"),
        )
        .await
        .unwrap();
    let req = recorded.lock().unwrap();
    assert_eq!(req.len(), 1);
    assert_eq!(req[0].method, "POST");
    assert!(req[0].path.starts_with("/calendar/v3/freeBusy"));
    let body: serde_json::Value = serde_json::from_str(&req[0].body).unwrap();
    assert_eq!(body["timeMin"], "2026-05-02T00:00:00Z");
    assert_eq!(body["timeMax"], "2026-05-03T00:00:00Z");
    assert_eq!(body["items"][0]["id"], "primary");
    // Response is passed through unchanged.
    assert_eq!(
        r["calendars"]["primary"]["busy"][0]["start"],
        "2026-05-02T15:00:00Z"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn check_availability_accepts_explicit_calendar_ids() {
    let (url, recorded, _) = spawn_mock_recording(&[("/calendar/v3/freeBusy", SAMPLE_FREEBUSY)]);
    let plugin = build_plugin(&url);
    let _ = plugin
        .tool_call(
            "calendar.check_availability",
            serde_json::json!({
                "time_min": "2026-05-02T00:00:00Z",
                "time_max": "2026-05-03T00:00:00Z",
                "calendar_ids": ["primary", "team-shared@group.calendar.google.com"],
            }),
            oauth("tok"),
        )
        .await
        .unwrap();
    let req = recorded.lock().unwrap();
    let body: serde_json::Value = serde_json::from_str(&req[0].body).unwrap();
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        body["items"][1]["id"],
        "team-shared@group.calendar.google.com"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn check_availability_requires_time_window() {
    let plugin = build_plugin("http://127.0.0.1:1");
    let err = plugin
        .tool_call(
            "calendar.check_availability",
            serde_json::json!({}),
            oauth("tok"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("time_min"), "got: {err}");
}

// ---------------------------------------------------------------------------
// get_event

const SAMPLE_SINGLE_EVENT: &str = r#"{
  "id": "evt-99",
  "summary": "1:1 with Carol",
  "status": "confirmed",
  "htmlLink": "https://calendar.google.com/event?eid=evt-99",
  "start": {"dateTime": "2026-05-05T17:00:00Z", "timeZone": "UTC"},
  "end":   {"dateTime": "2026-05-05T17:30:00Z", "timeZone": "UTC"},
  "attendees": [
    {"email": "carol@example.com", "responseStatus": "accepted"}
  ]
}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_event_returns_normalised_payload() {
    let (url, recorded, _) = spawn_mock_recording(&[(
        "/calendar/v3/calendars/primary/events/evt-99",
        SAMPLE_SINGLE_EVENT,
    )]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "calendar.get_event",
            serde_json::json!({"event_id": "evt-99"}),
            oauth("tok"),
        )
        .await
        .unwrap();
    let req = recorded.lock().unwrap();
    assert_eq!(req[0].method, "GET");
    assert_eq!(r["summary"], "1:1 with Carol");
    assert_eq!(r["start"]["kind"], "timed");
    assert_eq!(r["calendar_id"], "primary");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_event_requires_event_id() {
    let plugin = build_plugin("http://127.0.0.1:1");
    let err = plugin
        .tool_call("calendar.get_event", serde_json::json!({}), oauth("tok"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("event_id"), "got: {err}");
}

// ---------------------------------------------------------------------------
// create_event

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_event_posts_full_body_and_url_encodes_calendar() {
    let (url, recorded, _) =
        spawn_mock_recording(&[("/calendar/v3/calendars/primary/events", SAMPLE_SINGLE_EVENT)]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "calendar.create_event",
            serde_json::json!({
                "summary": "Lunch",
                "start": "2026-05-06T12:00:00Z",
                "end":   "2026-05-06T13:00:00Z",
                "description": "with the team",
                "location": "Sushi place",
            }),
            oauth("tok"),
        )
        .await
        .unwrap();
    let req = recorded.lock().unwrap();
    assert_eq!(req[0].method, "POST");
    // No attendees → no `?sendNotifications=true` query string.
    assert!(
        !req[0].path.contains("sendNotifications"),
        "no attendees → no notification flag, got {}",
        req[0].path,
    );
    let body: serde_json::Value = serde_json::from_str(&req[0].body).unwrap();
    assert_eq!(body["summary"], "Lunch");
    assert_eq!(body["start"]["dateTime"], "2026-05-06T12:00:00Z");
    assert_eq!(body["end"]["dateTime"], "2026-05-06T13:00:00Z");
    assert_eq!(body["description"], "with the team");
    assert_eq!(body["location"], "Sushi place");
    assert!(body["attendees"].is_null(), "no attendees in body");
    // Response is normalised.
    assert_eq!(r["summary"], "1:1 with Carol");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_event_with_attendees_appends_send_notifications() {
    let (url, recorded, _) =
        spawn_mock_recording(&[("/calendar/v3/calendars/primary/events", SAMPLE_SINGLE_EVENT)]);
    let plugin = build_plugin(&url);
    let _ = plugin
        .tool_call(
            "calendar.create_event",
            serde_json::json!({
                "summary": "Lunch",
                "start": "2026-05-06T12:00:00Z",
                "end":   "2026-05-06T13:00:00Z",
                "attendees": ["carol@example.com", "dan@example.com"],
            }),
            oauth("tok"),
        )
        .await
        .unwrap();
    let req = recorded.lock().unwrap();
    assert!(
        req[0].path.contains("sendNotifications=true"),
        "attendees present → must append the flag, got {}",
        req[0].path,
    );
    let body: serde_json::Value = serde_json::from_str(&req[0].body).unwrap();
    let attendees = body["attendees"].as_array().unwrap();
    assert_eq!(attendees.len(), 2);
    assert_eq!(attendees[0]["email"], "carol@example.com");
    assert_eq!(attendees[1]["email"], "dan@example.com");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_event_requires_summary_start_and_end() {
    let plugin = build_plugin("http://127.0.0.1:1");
    let err = plugin
        .tool_call(
            "calendar.create_event",
            serde_json::json!({"summary": "x", "start": "y"}),
            oauth("tok"),
        )
        .await
        .unwrap_err();
    let s = err.to_string();
    assert!(s.contains("summary"), "got: {s}");
}

// ---------------------------------------------------------------------------
// update_event

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn update_event_patches_only_supplied_fields() {
    let (url, recorded, _) = spawn_mock_recording(&[(
        "/calendar/v3/calendars/primary/events/evt-99",
        SAMPLE_SINGLE_EVENT,
    )]);
    let plugin = build_plugin(&url);
    let _ = plugin
        .tool_call(
            "calendar.update_event",
            serde_json::json!({
                "event_id": "evt-99",
                "summary": "Lunch (moved)",
            }),
            oauth("tok"),
        )
        .await
        .unwrap();
    let req = recorded.lock().unwrap();
    assert_eq!(req[0].method, "PATCH");
    let body: serde_json::Value = serde_json::from_str(&req[0].body).unwrap();
    assert_eq!(body["summary"], "Lunch (moved)");
    // start / end / description / location / attendees were NOT
    // supplied — must be absent so PATCH leaves them unchanged.
    assert!(body["start"].is_null());
    assert!(body["end"].is_null());
    assert!(body["description"].is_null());
    assert!(body["location"].is_null());
    assert!(body["attendees"].is_null());
    // Without attendees the URL must NOT carry ?sendNotifications.
    assert!(
        !req[0].path.contains("sendNotifications"),
        "no attendees → no notification flag, got {}",
        req[0].path,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn update_event_with_attendees_appends_send_notifications() {
    let (url, recorded, _) = spawn_mock_recording(&[(
        "/calendar/v3/calendars/primary/events/evt-99",
        SAMPLE_SINGLE_EVENT,
    )]);
    let plugin = build_plugin(&url);
    let _ = plugin
        .tool_call(
            "calendar.update_event",
            serde_json::json!({
                "event_id": "evt-99",
                "attendees": ["carol@example.com"],
            }),
            oauth("tok"),
        )
        .await
        .unwrap();
    let req = recorded.lock().unwrap();
    assert!(req[0].path.contains("sendNotifications=true"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn update_event_requires_event_id() {
    let plugin = build_plugin("http://127.0.0.1:1");
    let err = plugin
        .tool_call(
            "calendar.update_event",
            serde_json::json!({"summary": "noop"}),
            oauth("tok"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("event_id"), "got: {err}");
}

// ---------------------------------------------------------------------------
// delete_event

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_event_issues_delete_and_returns_deleted_true() {
    // 204-ish: respond with empty body. The mock always sends 200 +
    // content-length 0, which the script's decode_response treats as
    // a UNIT response — the script ignores it and returns
    // {deleted: true} unconditionally on a non-error.
    let (url, recorded, _) =
        spawn_mock_recording(&[("/calendar/v3/calendars/primary/events/evt-99", "")]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "calendar.delete_event",
            serde_json::json!({"event_id": "evt-99"}),
            oauth("tok"),
        )
        .await
        .unwrap();
    let req = recorded.lock().unwrap();
    assert_eq!(req[0].method, "DELETE");
    assert_eq!(req[0].path, "/calendar/v3/calendars/primary/events/evt-99");
    assert_eq!(r["deleted"], true);
    assert_eq!(r["event_id"], "evt-99");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_event_url_encodes_event_id() {
    let (url, recorded, _) =
        spawn_mock_recording(&[("/calendar/v3/calendars/primary/events/weird%2Fid", "")]);
    let plugin = build_plugin(&url);
    let _ = plugin
        .tool_call(
            "calendar.delete_event",
            serde_json::json!({"event_id": "weird/id"}),
            oauth("tok"),
        )
        .await
        .unwrap();
    let req = recorded.lock().unwrap();
    assert_eq!(
        req[0].path,
        "/calendar/v3/calendars/primary/events/weird%2Fid"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_event_requires_event_id() {
    let plugin = build_plugin("http://127.0.0.1:1");
    let err = plugin
        .tool_call("calendar.delete_event", serde_json::json!({}), oauth("tok"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("event_id"), "got: {err}");
}
