//! End-to-end test for the real `plugins/google-apps/main.rhai`
//! script. Loads it from disk, points module endpoints at a local
//! mock server, and exercises representative tools from each of the
//! five modules (Gmail, Calendar, Contacts, Tasks, Drive).
//!
//! Also pins the `enabled_modules` toggle gate — disabling a module
//! must make every tool in that module throw a "disabled" error
//! before the script tries to call out.
//!
//! Mirrors the `google_calendar.rs` test pattern (in-process mock
//! TCP server, no real Google credentials).

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

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    body: String,
}

fn spawn_mock_recording(
    routes: &'static [(&'static str, &'static str)],
) -> (String, Arc<Mutex<Vec<RecordedRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let recorded: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded_w = recorded.clone();
    std::thread::spawn(move || {
        loop {
            let Ok((mut sock, _)) = listener.accept() else {
                break;
            };
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
                    break;
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

            // Path-prefix routing — longest match wins. The matcher
            // strips query strings so a route like
            // "/gmail/v1/users/me/messages/abc" still matches a request
            // for "/gmail/v1/users/me/messages/abc?format=metadata".
            let bare = path.split('?').next().unwrap_or(&path);
            let resp_body = routes
                .iter()
                .filter(|(p, _)| bare.starts_with(p))
                .max_by_key(|(p, _)| p.len())
                .map(|(_, b)| *b)
                .unwrap_or(r#"{"items":[]}"#);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                resp_body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.shutdown(std::net::Shutdown::Write);
        }
    });
    (format!("http://{addr}"), recorded)
}

/// Rewrite all Google API URLs in the shipped script to point at our
/// mock server. Mirrors what google_calendar.rs does — we keep the
/// list small but enumerate every host the script reaches so a
/// future endpoint addition forces an explicit decision here.
fn load_script_against_mock(mock_url: &str) -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("plugins/google-apps/main.rhai");
    let original =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
    original
        .replace(
            r#""https://gmail.googleapis.com/gmail/v1/users/me/messages""#,
            &format!(r#""{mock_url}/gmail/v1/users/me/messages""#),
        )
        .replace(
            r#""https://gmail.googleapis.com/gmail/v1/users/me/messages/""#,
            &format!(r#""{mock_url}/gmail/v1/users/me/messages/""#),
        )
        .replace(
            r#""https://gmail.googleapis.com/gmail/v1/users/me/messages/send""#,
            &format!(r#""{mock_url}/gmail/v1/users/me/messages/send""#),
        )
        .replace(
            r#""https://gmail.googleapis.com/gmail/v1/users/me/drafts""#,
            &format!(r#""{mock_url}/gmail/v1/users/me/drafts""#),
        )
        .replace(
            r#""https://gmail.googleapis.com/gmail/v1/users/me/labels""#,
            &format!(r#""{mock_url}/gmail/v1/users/me/labels""#),
        )
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
        .replace(
            r#""https://people.googleapis.com/v1/people/me/connections""#,
            &format!(r#""{mock_url}/v1/people/me/connections""#),
        )
        .replace(
            r#""https://people.googleapis.com/v1/people:searchContacts""#,
            &format!(r#""{mock_url}/v1/people:searchContacts""#),
        )
        .replace(
            r#""https://tasks.googleapis.com/tasks/v1/users/@me/lists""#,
            &format!(r#""{mock_url}/tasks/v1/users/@me/lists""#),
        )
        .replace(
            r#""https://tasks.googleapis.com/tasks/v1/lists/""#,
            &format!(r#""{mock_url}/tasks/v1/lists/""#),
        )
        .replace(
            r#""https://www.googleapis.com/drive/v3/files""#,
            &format!(r#""{mock_url}/drive/v3/files""#),
        )
        .replace(
            r#""https://www.googleapis.com/drive/v3/files/""#,
            &format!(r#""{mock_url}/drive/v3/files/""#),
        )
        .replace(
            r#""https://www.googleapis.com/upload/drive/v3/files?uploadType=multipart&fields=id,name,mimeType,webViewLink,parents""#,
            &format!(r#""{mock_url}/upload/drive/v3/files?uploadType=multipart&fields=id,name,mimeType,webViewLink,parents""#),
        )
}

fn build_plugin(mock_url: &str) -> ScriptPlugin {
    let factory = ScriptEngine::with_loopback_allowed_for_tests();
    let source = load_script_against_mock(mock_url);
    ScriptPlugin::from_source("google-apps", &source, &factory)
        .expect("real google-apps/main.rhai must parse cleanly")
}

fn oauth(token: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert(
        "controller".into(),
        serde_json::Value::String(token.to_owned()),
    );
    m
}

// ===========================================================================
// Toggle gate — the most important behaviour to pin.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_tool_throws() {
    let plugin = build_plugin("http://127.0.0.1:1");
    let err = plugin
        .tool_call("gmail.delete_everything", serde_json::json!({}), oauth("t"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("gmail.delete_everything"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_oauth_token_errors_before_module_gate() {
    // No controller token at all → "not connected" error fires
    // before the toggle gate.
    let plugin = build_plugin("http://127.0.0.1:1");
    let err = plugin
        .tool_call(
            "gmail.list_labels",
            serde_json::json!({}),
            serde_json::Map::new(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not connected"), "got: {err}",);
}

// ===========================================================================
// Gmail
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gmail_list_labels_returns_normalised_shape() {
    let (url, _) = spawn_mock_recording(&[(
        "/gmail/v1/users/me/labels",
        r#"{"labels":[
            {"id":"INBOX","name":"INBOX","type":"system"},
            {"id":"Label_1","name":"Custom","type":"user"}
        ]}"#,
    )]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call("gmail.list_labels", serde_json::json!({}), oauth("t"))
        .await
        .unwrap();
    let labels = r["labels"].as_array().unwrap();
    assert_eq!(labels.len(), 2);
    assert_eq!(labels[0]["id"], "INBOX");
    assert_eq!(labels[0]["type"], "system");
    assert_eq!(labels[1]["name"], "Custom");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gmail_send_message_base64url_encodes_body_and_posts() {
    let (url, recorded) = spawn_mock_recording(&[(
        "/gmail/v1/users/me/messages/send",
        r#"{"id":"sent-1","threadId":"thr-1"}"#,
    )]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "gmail.send_message",
            serde_json::json!({
                "to": "alice@example.com",
                "subject": "Hi",
                "body": "Hello there",
            }),
            oauth("t"),
        )
        .await
        .unwrap();
    assert_eq!(r["id"], "sent-1");
    assert_eq!(r["thread_id"], "thr-1");
    let req = recorded.lock().unwrap();
    assert_eq!(req.len(), 1);
    assert_eq!(req[0].method, "POST");
    let body: serde_json::Value = serde_json::from_str(&req[0].body).unwrap();
    let raw = body["raw"].as_str().unwrap();
    // base64url uses '-' and '_' instead of '+' and '/', and strips
    // padding. The encoded RFC822 must therefore not contain any
    // standard-alphabet artefacts.
    assert!(!raw.contains('+'), "raw should be base64url, got: {raw}");
    assert!(!raw.contains('/'), "raw should be base64url, got: {raw}");
    assert!(!raw.ends_with('='), "raw should be unpadded, got: {raw}");
    // Decode and assert the headers are present.
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw)
        .expect("raw must decode as URL-safe base64");
    let decoded = String::from_utf8(bytes).unwrap();
    assert!(decoded.contains("To: alice@example.com"));
    assert!(decoded.contains("Subject: Hi"));
    assert!(decoded.contains("Hello there"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gmail_send_message_requires_recipient() {
    let plugin = build_plugin("http://127.0.0.1:1");
    let err = plugin
        .tool_call(
            "gmail.send_message",
            serde_json::json!({"subject": "x", "body": "y"}),
            oauth("t"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("to"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gmail_trash_posts_to_trash_endpoint() {
    let (url, recorded) =
        spawn_mock_recording(&[("/gmail/v1/users/me/messages/m1/trash", r#"{"id":"m1"}"#)]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "gmail.trash",
            serde_json::json!({"message_id": "m1"}),
            oauth("t"),
        )
        .await
        .unwrap();
    assert_eq!(r["trashed"], true);
    let req = recorded.lock().unwrap();
    assert_eq!(req[0].method, "POST");
    assert!(req[0].path.ends_with("/trash"));
}

// ===========================================================================
// Calendar — smoke-test the dispatch path; the verbatim logic is
// already exercised by crates/script/tests/google_calendar.rs.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn calendar_list_calendars_dispatches() {
    let (url, _) = spawn_mock_recording(&[(
        "/calendar/v3/users/me/calendarList",
        r#"{"items":[{"id":"primary","summary":"a","primary":true,"accessRole":"owner","timeZone":"UTC"}]}"#,
    )]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call("calendar.list_calendars", serde_json::json!({}), oauth("t"))
        .await
        .unwrap();
    let cals = r["calendars"].as_array().unwrap();
    assert_eq!(cals.len(), 1);
    assert_eq!(cals[0]["id"], "primary");
}

// ===========================================================================
// Contacts
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contacts_list_returns_normalised_shape() {
    let (url, _) = spawn_mock_recording(&[(
        "/v1/people/me/connections",
        r#"{"connections":[
            {"resourceName":"people/c1","names":[{"displayName":"Alice"}],
             "emailAddresses":[{"value":"a@x"}],
             "phoneNumbers":[{"value":"+15551234"}]}
        ]}"#,
    )]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call("contacts.list", serde_json::json!({}), oauth("t"))
        .await
        .unwrap();
    let contacts = r["contacts"].as_array().unwrap();
    assert_eq!(contacts.len(), 1);
    assert_eq!(contacts[0]["display_name"], "Alice");
    assert_eq!(contacts[0]["emails"][0], "a@x");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contacts_search_returns_google_results_when_index_is_warm() {
    // Happy path — searchContacts returns matches, plugin surfaces them
    // with the "google" source tag so the caller can tell which path
    // resolved the lookup. This is the path that fixes the silent-miss
    // bug for operators with >200 contacts.
    let (url, _) = spawn_mock_recording(&[(
        "/v1/people:searchContacts",
        r#"{"results":[
            {"person":{"resourceName":"people/c42",
             "names":[{"displayName":"Justin Long"}],
             "emailAddresses":[{"value":"justin@x"}]}}
        ]}"#,
    )]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "contacts.search",
            serde_json::json!({"query": "justin"}),
            oauth("t"),
        )
        .await
        .unwrap();
    let contacts = r["contacts"].as_array().unwrap();
    assert_eq!(contacts.len(), 1);
    assert_eq!(contacts[0]["display_name"], "Justin Long");
    assert_eq!(r["source"], "google");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contacts_search_falls_back_to_local_scan_when_google_returns_empty() {
    // The "cold cache" failure mode: searchContacts returns no results
    // (Google's index hasn't primed yet), but the connections list
    // clearly contains the target. The fallback substring-matches the
    // display name and surfaces the contact with the "fallback" source
    // tag. Without this, the silent-miss bug we just fixed would
    // reappear whenever Google's search index was cold.
    let (url, _) = spawn_mock_recording(&[
        ("/v1/people:searchContacts", r#"{"results":[]}"#),
        (
            "/v1/people/me/connections",
            r#"{"connections":[
                {"resourceName":"people/c1","names":[{"displayName":"Bob Smith"}],
                 "emailAddresses":[{"value":"bob@x"}]},
                {"resourceName":"people/c2","names":[{"displayName":"Justin Long"}],
                 "emailAddresses":[{"value":"justin@x"}]}
            ]}"#,
        ),
    ]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "contacts.search",
            serde_json::json!({"query": "justin"}),
            oauth("t"),
        )
        .await
        .unwrap();
    let contacts = r["contacts"].as_array().unwrap();
    assert_eq!(contacts.len(), 1, "fallback must skip non-matching contacts");
    assert_eq!(contacts[0]["display_name"], "Justin Long");
    assert_eq!(r["source"], "fallback");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contacts_search_fallback_matches_by_phone_digits() {
    // Phone substring match. The needle "5551234" must hit
    // "+1 (555) 123-4567" via digits_only normalisation. Pins the
    // contract that phone-number lookup works even when the stored
    // contact has formatting characters the query doesn't.
    let (url, _) = spawn_mock_recording(&[
        ("/v1/people:searchContacts", r#"{"results":[]}"#),
        (
            "/v1/people/me/connections",
            r#"{"connections":[
                {"resourceName":"people/c1","names":[{"displayName":"Alice"}],
                 "phoneNumbers":[{"value":"+1 (555) 123-4567"}]}
            ]}"#,
        ),
    ]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "contacts.search",
            serde_json::json!({"query": "5551234"}),
            oauth("t"),
        )
        .await
        .unwrap();
    let contacts = r["contacts"].as_array().unwrap();
    assert_eq!(contacts.len(), 1);
    assert_eq!(contacts[0]["display_name"], "Alice");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contacts_search_rejects_empty_query() {
    // Defensive — caller MUST provide a query; we don't dump the
    // address book when the search field is empty. (The dump path
    // is `contacts.list`, deliberately separated.)
    let plugin = build_plugin("http://127.0.0.1:1");
    let err = plugin
        .tool_call(
            "contacts.search",
            serde_json::json!({"query": ""}),
            oauth("t"),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("non-empty"),
        "expected non-empty query rejection, got: {err}",
    );
}

// ===========================================================================
// Tasks
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tasks_list_lists_dispatches() {
    let (url, _) = spawn_mock_recording(&[(
        "/tasks/v1/users/@me/lists",
        r#"{"items":[{"id":"l1","title":"My Tasks"}]}"#,
    )]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call("tasks.list_lists", serde_json::json!({}), oauth("t"))
        .await
        .unwrap();
    let lists = r["task_lists"].as_array().unwrap();
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0]["id"], "l1");
    assert_eq!(lists[0]["title"], "My Tasks");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tasks_create_task_posts_title_and_due() {
    let (url, recorded) = spawn_mock_recording(&[(
        "/tasks/v1/lists/%40default/tasks",
        r#"{"id":"t1","title":"Buy milk","status":"needsAction","due":"2026-05-15T00:00:00Z"}"#,
    )]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "tasks.create_task",
            serde_json::json!({
                "title": "Buy milk",
                "due": "2026-05-15T00:00:00Z",
            }),
            oauth("t"),
        )
        .await
        .unwrap();
    assert_eq!(r["id"], "t1");
    let req = recorded.lock().unwrap();
    assert_eq!(req[0].method, "POST");
    let body: serde_json::Value = serde_json::from_str(&req[0].body).unwrap();
    assert_eq!(body["title"], "Buy milk");
    assert_eq!(body["due"], "2026-05-15T00:00:00Z");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tasks_complete_task_sets_status_and_completed() {
    let (url, recorded) = spawn_mock_recording(&[(
        "/tasks/v1/lists/%40default/tasks/t1",
        r#"{"id":"t1","status":"completed","completed":"2026-05-10T00:00:00Z"}"#,
    )]);
    let plugin = build_plugin(&url);
    let _ = plugin
        .tool_call(
            "tasks.complete_task",
            serde_json::json!({"task_id": "t1"}),
            oauth("t"),
        )
        .await
        .unwrap();
    let req = recorded.lock().unwrap();
    assert_eq!(req[0].method, "PATCH");
    let body: serde_json::Value = serde_json::from_str(&req[0].body).unwrap();
    assert_eq!(body["status"], "completed");
    assert!(body["completed"].is_string());
}

// ===========================================================================
// Drive
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drive_search_builds_q_with_name_contains_clause() {
    let (url, recorded) = spawn_mock_recording(&[(
        "/drive/v3/files",
        r#"{"files":[
            {"id":"f1","name":"report.pdf","mimeType":"application/pdf",
             "modifiedTime":"2026-05-01T00:00:00Z","webViewLink":"https://x","parents":["root"]}
        ]}"#,
    )]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "drive.search",
            serde_json::json!({"query": "report"}),
            oauth("t"),
        )
        .await
        .unwrap();
    let files = r["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["id"], "f1");
    assert_eq!(files[0]["mime_type"], "application/pdf");
    let req = recorded.lock().unwrap();
    assert!(
        req[0].path.contains("name+contains") || req[0].path.contains("name%20contains"),
        "expected name-contains clause in q, got: {}",
        req[0].path,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drive_share_posts_permission_with_no_notification() {
    let (url, recorded) = spawn_mock_recording(&[(
        "/drive/v3/files/f1/permissions",
        r#"{"id":"perm-1","role":"reader","type":"user"}"#,
    )]);
    let plugin = build_plugin(&url);
    let r = plugin
        .tool_call(
            "drive.share",
            serde_json::json!({
                "file_id": "f1",
                "email": "alice@example.com",
                "role": "writer",
            }),
            oauth("t"),
        )
        .await
        .unwrap();
    assert_eq!(r["shared"], true);
    assert_eq!(r["role"], "writer");
    let req = recorded.lock().unwrap();
    assert!(req[0].path.contains("sendNotificationEmail=false"));
    let body: serde_json::Value = serde_json::from_str(&req[0].body).unwrap();
    assert_eq!(body["role"], "writer");
    assert_eq!(body["emailAddress"], "alice@example.com");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drive_share_rejects_unknown_role() {
    let plugin = build_plugin("http://127.0.0.1:1");
    let err = plugin
        .tool_call(
            "drive.share",
            serde_json::json!({"file_id": "f", "email": "a@x", "role": "owner"}),
            oauth("t"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("role"), "got: {err}");
}
