//! WebSocket event bus — the live stream the chat UI subscribes to.
//!
//! Wire shape matches `spec/asyncapi.yaml`:
//!
//! - `chat.message_inbound` / `chat.message_outbound` — user and agent text
//! - `chat.token_delta` — streaming assistant tokens
//! - `agent.tool_use` / `agent.tool_result` — every tool call
//! - `conversation.phase_changed` — FSM transitions
//! - `alert.*` — operational alerts
//!
//! Publishers call [`broadcast`] with a typed payload; the WebSocket
//! handler ([`stream_handler`]) forwards serialized events to every
//! connected subscriber.
//!
//! **No cloud dependencies.** `tokio::sync::broadcast` in-process only.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::state::AppState;

/// Every event surface-able to the UI. Additive — new variants land
/// without breaking subscribers since the UI tolerates unknown kinds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiEvent {
    /// Heartbeat so the client can detect dead connections.
    Ping {
        ts: i64,
    },

    /// User sent a message to a conversation.
    ChatMessageInbound {
        conversation_id: String,
        seq: i64,
        text: String,
        sender: Option<String>,
    },
    /// Agent text reply (full, non-streaming).
    ChatMessageOutbound {
        conversation_id: String,
        seq: i64,
        text: String,
    },
    /// Incremental token during a streaming turn.
    ChatTokenDelta {
        conversation_id: String,
        text: String,
    },

    AgentToolUse {
        conversation_id: String,
        seq: i64,
        tool_name: String,
        ordinal: u32,
    },
    AgentToolResult {
        conversation_id: String,
        seq: i64,
        ordinal: u32,
        ok: bool,
    },

    ConversationPhaseChanged {
        conversation_id: String,
        phase: String,
    },

    AlertFired {
        alert_id: String,
        severity: String,
        source: String,
        title: String,
    },
    AlertResolved {
        alert_id: String,
    },

    /// A routine run-history row's status changed (queued, picked up,
    /// finished). Drives live updates in the Settings → Routines
    /// run-history drawer so the operator doesn't have to refresh
    /// after a manual fire or to watch a scheduled fire complete.
    RoutineRunChanged {
        routine_id: String,
        run_id: String,
        /// Mirrors `RoutineRunStatus` — one of "Pending" | "Success"
        /// | "Failed" | "Skipped".
        status: String,
    },

    // ---- Phase 13.B — voice session lifecycle ----------------

    /// A voice session opened — first frame for the given session
    /// id arrived. Consumed by the SPA to render a "live mic" UX.
    VoiceSessionStarted {
        session: String,
        codec: String,
        sample_rate: u32,
    },
    /// A voice session ended either because the operator hit stop
    /// or because the server's stale-session reaper aged it out.
    VoiceSessionEnded {
        session: String,
        /// One of "operator_stopped" | "stale_reap" |
        /// "transport_closed".
        reason: String,
    },
    /// Streaming partial transcript. The Whisper adapter (Phase
    /// 13.C) feeds these as text accumulates; SPA renders them
    /// inline with the typing-indicator UX.
    VoiceTranscript {
        session: String,
        seq: u32,
        text: String,
        /// True when the segment is the final transcript (the
        /// endpointer fired and the LLM call is being dispatched).
        /// SPA uses this to commit the partial to the chat log.
        is_final: bool,
    },
    /// Outbound TTS audio chunk for the SPA to play. Phase 13.C
    /// fills these with real Kokoro output; today this variant
    /// exists so the SPA's WS handler can be wired ahead of the
    /// adapter landing.
    VoiceAudioOutbound {
        session: String,
        seq: u32,
        /// Codec the bytes are encoded in. SPA's audio pipeline
        /// picks the right MediaSource decoder.
        codec: String,
        /// base64 of the codec payload — keeps the WS event JSON
        /// inspectable in dev tools. Binary out lands when the
        /// SPA's audio pipeline is ready for raw frames.
        audio_b64: String,
    },
    /// Operator (or VAD) interrupted the agent mid-response. The
    /// SPA flushes any in-flight audio playback; the runner halts
    /// the LLM stream and TTS synthesis.
    VoiceInterrupted {
        session: String,
        /// One of "operator_barge_in" | "vad_speech_detected".
        reason: String,
    },
}

/// Broadcast channel capacity. Lagging subscribers drop the oldest
/// events (broadcast's default behavior), which is fine — the UI uses
/// the event log for canonical state.
const CHANNEL_CAPACITY: usize = 256;

/// Handle that publishers use to fan-out `UiEvent`s to every live
/// WebSocket subscriber.
#[derive(Clone, Debug)]
pub struct EventBus {
    tx: broadcast::Sender<UiEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Broadcast an event. Swallows "no receivers" since it's expected
    /// to happen before any UI connects.
    pub fn publish(&self, ev: UiEvent) {
        let _ = self.tx.send(ev);
    }

    /// Create a new subscriber. Returns the receiver only; callers must
    /// hold it for the duration they want to observe events.
    pub fn subscribe(&self) -> broadcast::Receiver<UiEvent> {
        self.tx.subscribe()
    }

    /// Current subscriber count — useful for health probes.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// WebSocket handler
// ---------------------------------------------------------------------------

/// `GET /api/stream` — upgrades to a WebSocket that pushes every
/// [`UiEvent`] to the connected client. Accepts incoming text messages
/// only for pings (ignored in Phase 1; reserved for future bidirectional
/// control messages).
pub async fn stream_handler(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let bus = state.events.clone();
    let voice = state.voice_sessions.clone();
    let runtime = state.voice_runtime.clone();
    ws.on_upgrade(move |socket| handle_socket(socket, bus, voice, runtime))
}

async fn handle_socket(
    mut socket: WebSocket,
    bus: EventBus,
    voice: crate::voice_session::VoiceSessionRegistry,
    runtime: crate::voice_runtime::VoiceRuntime,
) {
    let mut rx = bus.subscribe();
    debug!(
        "ws stream connected; subscribers now {}",
        bus.subscriber_count()
    );

    // Send an initial ping so the client knows the stream is live.
    let _ = socket
        .send(Message::Text(
            serde_json::to_string(&UiEvent::Ping {
                ts: chrono::Utc::now().timestamp(),
            })
            .unwrap_or_else(|_| "{}".into())
            .into(),
        ))
        .await;

    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ui_ev) => {
                    let payload = match serde_json::to_string(&ui_ev) {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("ws serialize failed: {e}");
                            continue;
                        }
                    };
                    if socket.send(Message::Text(payload.into())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("ws lagged by {n} events; client will see gap");
                    // keep going; client should re-hydrate from the event log
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(Message::Binary(bytes))) => {
                    // Phase 13.B — parse the framing header, then
                    // hand the chunk to the voice-session registry.
                    // Phase 13.C — the registry's released chunks
                    // are forwarded into `voice_runtime.ingest_chunks`,
                    // which feeds the real Whisper adapter.
                    match crate::voice_frame::parse_frame(&bytes) {
                        Ok((header, payload)) => {
                            let outcome = voice.observe_frame(&header, payload).await;
                            voice.publish_outcome(&header, &outcome);
                            if !outcome.released.is_empty() {
                                runtime.ingest_chunks(&outcome.released).await;
                            }
                        }
                        Err(e) => {
                            warn!(
                                bytes = bytes.len(),
                                "ws voice frame parse failed: {e}"
                            );
                        }
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    // Phase 13.C / 13.D — voice control messages.
                    // Schema:
                    //   {"op":"voice_stop","session":"…"}
                    //   {"op":"voice_interrupt","session":"…"}
                    // Anything else is ignored. The control surface
                    // is intentionally tiny so a malformed message
                    // can't tear down the WS.
                    handle_voice_control(&text, &voice, &runtime).await;
                }
                Some(Ok(_)) => {} // ignore other client→server traffic
                Some(Err(e)) => {
                    warn!("ws recv error: {e}");
                    break;
                }
            }
        }
    }
    debug!("ws stream disconnected");
}

/// Phase 13.C/D — parse + dispatch a voice-control text message.
/// Public for unit testing; the WS handler calls it directly.
pub async fn handle_voice_control(
    text: &str,
    voice: &crate::voice_session::VoiceSessionRegistry,
    runtime: &crate::voice_runtime::VoiceRuntime,
) {
    let parsed: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return, // not JSON — silently ignore (could be a future text op)
    };
    let op = parsed.get("op").and_then(|v| v.as_str()).unwrap_or("");
    let session = parsed
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if session.is_empty() {
        return;
    }
    match op {
        "voice_stop" => {
            // Operator toggled mic off. Flush STT, then run the
            // agent reply path. The agent callback is wired in the
            // chat layer (Phase 13.C+); for v1 we stub with the
            // identity transcript so the pipeline is observable
            // end-to-end without yet plumbing into the chat router.
            // The real agent_reply hookup lands in 13.D.
            //
            // The finalize_utterance + drop_session run sequentially
            // — finalize awaits the agent reply + TTS, drop_session
            // can only run once that returns or there's a race
            // between the cleanup and an in-flight synthesize.
            let session_id = session.to_owned();
            let runtime_for_finalize = runtime.clone();
            let runtime_for_cleanup = runtime.clone();
            let voice_for_cleanup = voice.clone();
            tokio::spawn(async move {
                runtime_for_finalize
                    .finalize_utterance(&session_id, |transcript| async move {
                        // Placeholder: echo the transcript so the
                        // SPA can verify the round-trip. Phase 13.D
                        // wires this to the runner / chat path.
                        format!("you said: {transcript}")
                    })
                    .await;
                voice_for_cleanup
                    .end_session(&session_id, "operator_stopped")
                    .await;
                runtime_for_cleanup.drop_session(&session_id).await;
            });
        }
        "voice_interrupt" => {
            // Operator (or future server-side VAD) interrupted the
            // agent's reply mid-stream.
            runtime
                .interrupt(session, "operator_barge_in")
                .await;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bus_publishes_to_subscribers() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(UiEvent::Ping { ts: 42 });
        let got = rx.recv().await.unwrap();
        match got {
            UiEvent::Ping { ts } => assert_eq!(ts, 42),
            other => panic!("wrong kind: {other:?}"),
        }
    }

    #[tokio::test]
    async fn bus_tolerates_no_subscribers() {
        let bus = EventBus::new();
        // No panic, no error.
        bus.publish(UiEvent::Ping { ts: 1 });
    }

    #[test]
    fn ui_event_tag_is_snake_case() {
        let e = UiEvent::ChatMessageInbound {
            conversation_id: "c".into(),
            seq: 1,
            text: "hi".into(),
            sender: None,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"kind\":\"chat_message_inbound\""));
    }

    #[tokio::test]
    async fn subscriber_count_tracks_live_subscribers() {
        let bus = EventBus::new();
        assert_eq!(bus.subscriber_count(), 0);
        let _a = bus.subscribe();
        let _b = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 2);
        drop(_a);
        assert_eq!(bus.subscriber_count(), 1);
    }

    // ---- Phase 13.C — voice control message dispatch -------------

    use crate::voice_runtime::{SttFactory, TtsFactory, VoiceRuntime};
    use crate::voice_session::VoiceSessionRegistry;
    use execlaw_voice_pipeline::traits::{MockStt, MockTts, TtsClient};
    use std::sync::Arc;
    use std::time::Duration;

    fn mock_runtime(bus: EventBus) -> VoiceRuntime {
        let stt: SttFactory = Arc::new(|| {
            Box::new(MockStt::new(Vec::new(), "transcribed!".to_owned()))
        });
        let tts: TtsFactory =
            Arc::new(|| (Box::new(MockTts::default()) as Box<dyn TtsClient>, None));
        VoiceRuntime::new(bus, stt, tts)
    }

    #[tokio::test]
    async fn handle_voice_control_ignores_non_json_text() {
        let bus = EventBus::new();
        let voice = VoiceSessionRegistry::new(bus.clone());
        let runtime = mock_runtime(bus);
        // Should not panic.
        handle_voice_control("not json", &voice, &runtime).await;
    }

    #[tokio::test]
    async fn handle_voice_control_ignores_unknown_op() {
        let bus = EventBus::new();
        let voice = VoiceSessionRegistry::new(bus.clone());
        let runtime = mock_runtime(bus);
        handle_voice_control(
            r#"{"op":"voice_yodel","session":"s1"}"#,
            &voice,
            &runtime,
        )
        .await;
        // No assertion target — pass if no panic + no event. The
        // shape of "did nothing" is captured by the lack of side
        // effects in subsequent tests.
    }

    #[tokio::test]
    async fn handle_voice_control_drops_messages_without_session() {
        let bus = EventBus::new();
        let voice = VoiceSessionRegistry::new(bus.clone());
        let runtime = mock_runtime(bus);
        handle_voice_control(r#"{"op":"voice_stop"}"#, &voice, &runtime).await;
    }

    #[tokio::test]
    async fn voice_interrupt_op_publishes_voice_interrupted() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let voice = VoiceSessionRegistry::new(bus.clone());
        let runtime = mock_runtime(bus);
        // Need a live runtime session for interrupt to do anything.
        // Create one by ingesting a chunk first.
        let chunk = crate::voice_session::OrderedAudioChunk {
            session: "s1".into(),
            seq: 0,
            codec: "pcm16le".into(),
            sample_rate: 16_000,
            channels: 1,
            payload: vec![0u8; 32],
        };
        runtime.ingest_chunks(&[chunk]).await;
        handle_voice_control(
            r#"{"op":"voice_interrupt","session":"s1"}"#,
            &voice,
            &runtime,
        )
        .await;
        let mut saw = false;
        for _ in 0..16 {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(UiEvent::VoiceInterrupted { session, reason })) => {
                    if session == "s1" && reason == "operator_barge_in" {
                        saw = true;
                        break;
                    }
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(saw, "VoiceInterrupted must publish on voice_interrupt op");
    }

    #[tokio::test]
    async fn voice_stop_op_finalizes_and_ends_session() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let voice = VoiceSessionRegistry::new(bus.clone());
        let runtime = mock_runtime(bus);
        // Open the session via the registry path so the SPA-facing
        // VoiceSessionStarted/Ended sequence is real.
        let header = crate::voice_frame::VoiceFrameHeader {
            session: "s1".into(),
            seq: 0,
            codec: "pcm16le".into(),
            sample_rate: 16_000,
            channels: 1,
            ts_ms: None,
        };
        let outcome = voice.observe_frame(&header, &[0u8; 32]).await;
        runtime.ingest_chunks(&outcome.released).await;
        handle_voice_control(
            r#"{"op":"voice_stop","session":"s1"}"#,
            &voice,
            &runtime,
        )
        .await;
        // The voice_stop branch spawns a task; give it a moment.
        let mut saw_final_transcript = false;
        let mut saw_session_end = false;
        for _ in 0..50 {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(UiEvent::VoiceTranscript { is_final, text, .. }))
                    if is_final && text == "transcribed!" =>
                {
                    saw_final_transcript = true;
                }
                Ok(Ok(UiEvent::VoiceSessionEnded { session, reason }))
                    if session == "s1" && reason == "operator_stopped" =>
                {
                    saw_session_end = true;
                }
                Ok(Ok(_)) => continue,
                _ => {}
            }
            if saw_final_transcript && saw_session_end {
                break;
            }
        }
        assert!(saw_final_transcript, "voice_stop must publish a final transcript");
        assert!(saw_session_end, "voice_stop must end the registry session");
    }
}
