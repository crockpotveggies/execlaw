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
    ws.on_upgrade(move |socket| handle_socket(socket, state.events.clone()))
}

async fn handle_socket(mut socket: WebSocket, bus: EventBus) {
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
                    // Phase 13.A — binary inbound frames are the
                    // operator's microphone audio for voice mode.
                    // The full pipeline (VAD → STT → LLM → TTS) lands
                    // in 13.B onward; for now we just log so
                    // operators can confirm the SPA's MediaRecorder
                    // bytes actually reach the server. Drops the
                    // bytes; no transcription, no event log
                    // pollution.
                    debug!(
                        bytes = bytes.len(),
                        "ws voice frame received (stub: discarded until 13.B)"
                    );
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
}
