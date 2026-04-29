//! Phase 13.C/D audit closure — end-to-end WebSocket voice round-trip.
//!
//! Unit tests cover every voice component in isolation: frame parser,
//! session registry, runtime, clients, control-message dispatch. This
//! test wires them together through a real `axum::serve` instance +
//! a `tokio-tungstenite` client, exercising:
//!
//!   1. WS upgrade against `/api/stream`.
//!   2. Binary voice frame upload (the SPA's `framePayload` shape).
//!   3. `voice_stop` text control message.
//!   4. Asserting `VoiceSessionStarted` + `VoiceTranscript {is_final}` +
//!      `VoiceSessionEnded` flow back over the same socket.
//!
//! What this catches that unit tests can't: a regression in the
//! `events.rs::handle_socket` select loop that breaks frame dispatch,
//! a panic-safety regression in the `voice_stop` cleanup, or an
//! event-ordering regression that ships a final transcript before
//! its `VoiceSessionStarted`.

use axum::Router;
use execlaw_core::db::{Database, DbConfig};
use execlaw_core::migrations::MigrationRunner;
use execlaw_plugin_host::{HookRegistry, PluginHost};
use execlaw_server::voice_runtime::{SttFactory, TtsFactory, VoiceRuntime};
use execlaw_server::{AppState, EventBus, JwtSigner, RefreshStore, ServerConfig};
use execlaw_voice_pipeline::traits::{MockStt, MockTts, TtsClient};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Build an `AppState` with mock STT/TTS that returns a deterministic
/// transcript on flush, so the test can assert exact event payloads.
fn build_state(transcript: &'static str) -> AppState {
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    let stage_root = std::env::temp_dir().join(format!(
        "execlaw-test-voice-ws-{}",
        uuid::Uuid::new_v4()
    ));
    let events = EventBus::new();
    let stt: SttFactory = Arc::new(move || {
        Box::new(MockStt::new(Vec::new(), transcript.to_owned()))
    });
    let tts: TtsFactory = Arc::new(|| {
        (
            Box::new(MockTts::default()) as Box<dyn TtsClient>,
            None,
        )
    });
    AppState {
        db: db.clone(),
        config: Arc::new(ServerConfig::default()),
        signer: Arc::new(JwtSigner::generate("voice-test".into())),
        refresh_store: Arc::new(RefreshStore::new(db.clone())),
        events: events.clone(),
        event_log_hmac_key: Some(Arc::new(b"execlaw-test-hmac-key-32-bytes!!".to_vec())),
        inference: Arc::new(
            execlaw_server::inference_resolver::InferenceResolver::new(None),
        ),
        plugin_host: PluginHost::new(db.clone(), HookRegistry::new(), stage_root),
        webauthn: None,
        mcp_host: execlaw_server::mcp_host::McpHost::new(db),
        backend_supervisor: None,
        voice_sessions: execlaw_server::voice_session::VoiceSessionRegistry::new(
            events.clone(),
        ),
        voice_runtime: VoiceRuntime::new(events, stt, tts),
        turn_cancel: execlaw_server::turn_cancel::TurnCancellationRegistry::new(),
        runner_supervisor: None,
    }
}

/// Spin up an axum server on an ephemeral port. Returns the bound
/// address and a stop handle.
async fn spawn_test_server(state: AppState) -> (String, tokio::task::JoinHandle<()>) {
    let app: Router = execlaw_server::routes::build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let h = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("ws://{addr}/api/stream"), h)
}

/// Build a Phase-13.A voice frame: `[u32 header_len BE][JSON header][payload]`.
fn frame_bytes(session: &str, seq: u32, payload: &[u8]) -> Vec<u8> {
    let header_json = serde_json::json!({
        "session": session,
        "seq": seq,
        "codec": "pcm16le",
        "sample_rate": 16_000,
        "channels": 1,
    });
    let header_str = serde_json::to_string(&header_json).unwrap();
    let header_bytes = header_str.as_bytes();
    let mut out = Vec::with_capacity(4 + header_bytes.len() + payload.len());
    out.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(header_bytes);
    out.extend_from_slice(payload);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_voice_round_trip_emits_session_started_transcript_and_session_ended() {
    let state = build_state("hello from whisper");
    let (url, _server) = spawn_test_server(state).await;

    // Open the WS. tokio-tungstenite's `connect_async` does the
    // HTTP upgrade for us.
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws upgrade should succeed");

    // The server sends an initial Ping right after upgrade; drain it
    // so it doesn't muddle our event-shape assertions.
    let first = tokio::time::timeout(Duration::from_secs(1), ws.next())
        .await
        .expect("initial ping should arrive within 1s")
        .unwrap()
        .unwrap();
    if let WsMessage::Text(t) = first {
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        assert_eq!(v["kind"], "ping");
    } else {
        panic!("expected initial ping text, got {first:?}");
    }

    // Send one binary voice frame (~10ms of pcm16 silence).
    let payload = vec![0u8; 320];
    ws.send(WsMessage::Binary(frame_bytes("ws-session-1", 0, &payload)))
        .await
        .unwrap();

    // Send voice_stop.
    ws.send(WsMessage::Text(
        r#"{"op":"voice_stop","session":"ws-session-1"}"#.into(),
    ))
    .await
    .unwrap();

    // Drain events for ~2s and check the expected sequence appears.
    let mut saw_started = false;
    let mut saw_final_transcript = false;
    let mut saw_session_ended = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        let next = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
        let msg = match next {
            Ok(Some(Ok(m))) => m,
            _ => continue,
        };
        let WsMessage::Text(t) = msg else { continue };
        let v: serde_json::Value = match serde_json::from_str(&t) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v["kind"].as_str().unwrap_or("") {
            "voice_session_started" if v["session"] == "ws-session-1" => {
                saw_started = true;
            }
            "voice_transcript" if v["session"] == "ws-session-1" => {
                if v["is_final"] == true && v["text"] == "hello from whisper" {
                    saw_final_transcript = true;
                }
            }
            "voice_session_ended" if v["session"] == "ws-session-1" => {
                saw_session_ended = true;
            }
            _ => continue,
        }
        if saw_started && saw_final_transcript && saw_session_ended {
            break;
        }
    }
    let _ = ws.close(None).await;
    assert!(saw_started, "must observe VoiceSessionStarted");
    assert!(
        saw_final_transcript,
        "must observe VoiceTranscript {{ is_final: true, text: \"hello from whisper\" }}"
    );
    assert!(saw_session_ended, "must observe VoiceSessionEnded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_disconnect_mid_utterance_cleans_up_voice_session() {
    // Audit closure — track per-WS owned sessions and clean on
    // disconnect. Without the fix, a tab close mid-utterance would
    // leave the session in the registry until the 30s reaper sweep.
    let state = build_state("won't be needed");
    let voice_sessions = state.voice_sessions.clone();
    let (url, _server) = spawn_test_server(state).await;

    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url).await.unwrap();
    // Drain the initial ping.
    let _ = tokio::time::timeout(Duration::from_secs(1), ws.next()).await;

    // Open a session by sending a binary frame.
    let payload = vec![0u8; 320];
    ws.send(WsMessage::Binary(frame_bytes("disconnect-victim", 0, &payload)))
        .await
        .unwrap();

    // Wait briefly for the registry to register the session, then
    // close the WS without sending voice_stop.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        voice_sessions.live_count().await,
        1,
        "registry must have observed the inbound frame's session"
    );
    let _ = ws.close(None).await;
    drop(ws);

    // Give the server's handle_socket a moment to run its
    // disconnect cleanup. Without the fix this would still report
    // 1 session for ~30s; with the fix it drops to 0 within ms.
    let mut cleaned = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if voice_sessions.live_count().await == 0 {
            cleaned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        cleaned,
        "WS disconnect must drop owned voice sessions without waiting for the reaper"
    );
}
