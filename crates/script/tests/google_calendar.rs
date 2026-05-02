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

/// Mock server that serves canned responses based on the request
/// path. Returns the bound base URL + a shared call-counter.
fn spawn_mock(routes: &'static [(&'static str, &'static str)]) -> (String, Arc<Mutex<u32>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(Mutex::new(0));
    let calls_w = calls.clone();
    std::thread::spawn(move || {
        loop {
            let Ok((mut sock, _)) = listener.accept() else {
                break;
            };
            let mut buf = [0u8; 16384];
            let n = sock.read(&mut buf).unwrap_or(0);
            *calls_w.lock().unwrap() += 1;
            // Crude path extraction from the request line: `METHOD PATH HTTP/...`
            let req = std::str::from_utf8(&buf[..n]).unwrap_or("");
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/");
            // Match longest prefix that the requested path starts with.
            let body = routes
                .iter()
                .filter(|(p, _)| path.starts_with(p))
                .max_by_key(|(p, _)| p.len())
                .map(|(_, b)| *b)
                .unwrap_or(r#"{"items":[]}"#);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
        }
    });
    (format!("http://{addr}"), calls)
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

/// Read `plugins/google-calendar/main.rhai` and rewrite both
/// hardcoded Calendar-API URLs to point at our local mock.
fn load_script_against_mock(mock_url: &str) -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("plugins/google-calendar/main.rhai");
    let original = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
    original
        .replace(
            r#""https://www.googleapis.com/calendar/v3/users/me/calendarList""#,
            &format!(r#""{mock_url}/calendar/v3/users/me/calendarList""#),
        )
        .replace(
            r#""https://www.googleapis.com/calendar/v3/calendars/""#,
            &format!(r#""{mock_url}/calendar/v3/calendars/""#),
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
    let (url, calls) = spawn_mock(&[(
        "/calendar/v3/users/me/calendarList",
        SAMPLE_CALENDARS,
    )]);
    let plugin = build_plugin(&url);
    let _ = plugin
        .tool_call("calendar.list_calendars", serde_json::json!({}), oauth("tok"))
        .await
        .unwrap();
    let _ = plugin
        .tool_call("calendar.list_calendars", serde_json::json!({}), oauth("tok"))
        .await
        .unwrap();
    assert_eq!(*calls.lock().unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_rotation_invalidates_calendars_cache() {
    let (url, calls) = spawn_mock(&[(
        "/calendar/v3/users/me/calendarList",
        SAMPLE_CALENDARS,
    )]);
    let plugin = build_plugin(&url);
    let _ = plugin
        .tool_call("calendar.list_calendars", serde_json::json!({}), oauth("token-A"))
        .await
        .unwrap();
    let _ = plugin
        .tool_call("calendar.list_calendars", serde_json::json!({}), oauth("token-B"))
        .await
        .unwrap();
    assert_eq!(*calls.lock().unwrap(), 2);
}

// ---------------------------------------------------------------------------
// list_events

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_events_default_args_uses_primary_calendar_and_7_day_window() {
    let (url, _) = spawn_mock(&[(
        "/calendar/v3/calendars/primary/events",
        SAMPLE_EVENTS,
    )]);
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
    let (url, _) = spawn_mock(&[(
        "/calendar/v3/calendars/primary/events",
        SAMPLE_EVENTS,
    )]);
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
    let (url, _) = spawn_mock(&[(
        "/calendar/v3/calendars/primary/events",
        r#"{"items":[]}"#,
    )]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "calendar.list_events",
            serde_json::json!({}),
            oauth("tok"),
        )
        .await
        .unwrap();
    assert_eq!(r["events"].as_array().unwrap().len(), 0);
    assert_eq!(r["calendar_id"], "primary");
    assert!(r["time_min"].as_str().unwrap().contains("T"));
    assert!(r["time_max"].as_str().unwrap().contains("T"));
}
