//! Per-flow-run event channel (M6).
//!
//! When an automation runs, the executor publishes `FlowChannelEvent`
//! values onto a broadcast channel keyed by `run_id`. The SPA's
//! Automations page subscribes for one run to render a live trace
//! (node-by-node progress + agent token deltas + reply-router
//! degradation notes).
//!
//! This is the Inngest-style "channel bus" pattern adapted to Rust:
//!   * Producers: AskAgent, SendReply, NodeStarted/Finished hooks
//!   * Consumers: SPA WebSocket / SSE clients (slice 7)
//!
//! Persistence is intentionally *out of scope* for the live channel
//! — it's an ephemeral broadcast. Long-lived trace is the existing
//! `state_automation_runs.step_traces` table, hydrated on page load.

use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use std::collections::HashMap;

/// One frame on a flow-run's live channel. Mirrors Anthropic's
/// block-delta model — text/tool/done deltas namespaced by the
/// node_id that emitted them, so a UI with multiple AskAgent nodes
/// in one flow can demultiplex correctly.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowChannelEvent {
    /// A node started executing. Carries the kind for client-side
    /// switch (e.g., render an animated typing indicator for
    /// AskAgent vs. a small step pill for Filter).
    NodeStarted {
        run_id: String,
        node_id: String,
        node_kind: String,
    },
    /// A node finished. `output` is the same JSON the trace records.
    NodeFinished {
        run_id: String,
        node_id: String,
        output: serde_json::Value,
        ms: u64,
        error: Option<String>,
    },
    /// AskAgent invocation started (i.e., the model HTTP call kicked
    /// off — distinct from `NodeStarted` which fires earlier, before
    /// template rendering + config parsing).
    AgentTurnStarted { run_id: String, node_id: String },
    /// One text-content delta from the model. `index = 0` for the
    /// initial content block; multi-block turns increment.
    AgentTextDelta {
        run_id: String,
        node_id: String,
        index: u32,
        text: String,
    },
    /// Tool-input streaming (Anthropic emits partial-JSON deltas
    /// for tool_use blocks as the model fills in arguments).
    AgentToolCallDelta {
        run_id: String,
        node_id: String,
        index: u32,
        name: String,
        input_json_partial: String,
    },
    /// AskAgent turn finished. `exit_tool` + `args` echo the
    /// node's recorded output for clients that joined mid-stream.
    AgentTurnFinished {
        run_id: String,
        node_id: String,
        exit_tool: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// SendReply's outcome — clients render this as a delivery
    /// status pill ("✓ delivered via whatsapp" / "↩ recovered to
    /// Inbox: <reason>").
    ReplyRouted {
        run_id: String,
        node_id: String,
        #[serde(flatten)]
        outcome: serde_json::Value,
    },
    /// The whole flow run terminated.
    RunFinished {
        run_id: String,
        outcome: String, // "success" | "skipped" | "failed"
    },
}

/// Broadcast hub keyed by `run_id`. Subscribers get a `Receiver`;
/// producers call `publish` to fan out. Channels self-prune when
/// the last subscriber drops.
#[derive(Clone)]
pub struct FlowChannelHub {
    inner: Arc<RwLock<HashMap<String, broadcast::Sender<FlowChannelEvent>>>>,
}

impl FlowChannelHub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Subscribe to a run's event stream. Late subscribers miss
    /// events emitted before subscription — clients that need
    /// historical trace should hit the
    /// `/api/admin/automations/:id/runs` endpoint for the persisted
    /// trace, then subscribe here for the live tail.
    pub async fn subscribe(&self, run_id: &str) -> broadcast::Receiver<FlowChannelEvent> {
        let mut map = self.inner.write().await;
        let sender = map.entry(run_id.to_owned()).or_insert_with(|| {
            // 128-event buffer — large enough to absorb a fast-streaming
            // agent's token burst without backing up the producer.
            let (tx, _rx) = broadcast::channel(128);
            tx
        });
        sender.subscribe()
    }

    /// Publish an event. Producers don't care if there are
    /// subscribers — flows publish unconditionally; the broadcast
    /// channel drops events with no listeners. Returns true if
    /// anyone was listening (useful for instrumentation).
    pub fn publish(&self, event: FlowChannelEvent) -> bool {
        let run_id = match &event {
            FlowChannelEvent::NodeStarted { run_id, .. }
            | FlowChannelEvent::NodeFinished { run_id, .. }
            | FlowChannelEvent::AgentTurnStarted { run_id, .. }
            | FlowChannelEvent::AgentTextDelta { run_id, .. }
            | FlowChannelEvent::AgentToolCallDelta { run_id, .. }
            | FlowChannelEvent::AgentTurnFinished { run_id, .. }
            | FlowChannelEvent::ReplyRouted { run_id, .. }
            | FlowChannelEvent::RunFinished { run_id, .. } => run_id.clone(),
        };
        // try_read so producers in spawn_blocking threads don't
        // await; if the lock is contended (very rare) we skip the
        // publish — the persisted trace covers the loss.
        let Ok(map) = self.inner.try_read() else {
            return false;
        };
        if let Some(sender) = map.get(&run_id) {
            sender.send(event).is_ok()
        } else {
            false
        }
    }

    /// Reap channels with no subscribers. Optional — useful to
    /// avoid the map growing unboundedly under a busy bus. Call
    /// periodically from a sweeper or on every `RunFinished`.
    pub async fn prune(&self) {
        let mut map = self.inner.write().await;
        map.retain(|_, sender| sender.receiver_count() > 0);
    }
}

impl Default for FlowChannelHub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscribe_then_publish_round_trips() {
        let hub = FlowChannelHub::new();
        let mut rx = hub.subscribe("run-1").await;
        let listened = hub.publish(FlowChannelEvent::NodeStarted {
            run_id: "run-1".into(),
            node_id: "f1".into(),
            node_kind: "Filter".into(),
        });
        assert!(listened);
        let evt = rx.recv().await.unwrap();
        match evt {
            FlowChannelEvent::NodeStarted { node_id, .. } => assert_eq!(node_id, "f1"),
            other => panic!("expected NodeStarted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn publish_to_unsubscribed_run_returns_false() {
        let hub = FlowChannelHub::new();
        let listened = hub.publish(FlowChannelEvent::RunFinished {
            run_id: "nobody".into(),
            outcome: "success".into(),
        });
        assert!(!listened);
    }

    #[tokio::test]
    async fn two_subscribers_both_receive() {
        let hub = FlowChannelHub::new();
        let mut a = hub.subscribe("run-2").await;
        let mut b = hub.subscribe("run-2").await;
        hub.publish(FlowChannelEvent::RunFinished {
            run_id: "run-2".into(),
            outcome: "success".into(),
        });
        let _ = a.recv().await.unwrap();
        let _ = b.recv().await.unwrap();
    }

    #[tokio::test]
    async fn events_are_namespaced_by_run_id() {
        let hub = FlowChannelHub::new();
        let mut a = hub.subscribe("run-a").await;
        let _b = hub.subscribe("run-b").await;
        hub.publish(FlowChannelEvent::RunFinished {
            run_id: "run-b".into(),
            outcome: "success".into(),
        });
        // a should NOT receive run-b's event
        assert!(a.try_recv().is_err());
    }

    #[tokio::test]
    async fn prune_clears_unsubscribed_channels() {
        let hub = FlowChannelHub::new();
        {
            let _rx = hub.subscribe("ephemeral").await;
            // rx dropped at end of block
        }
        hub.prune().await;
        // After prune, publishing should return false (no listeners
        // and no stored sender).
        let listened = hub.publish(FlowChannelEvent::RunFinished {
            run_id: "ephemeral".into(),
            outcome: "success".into(),
        });
        assert!(!listened);
    }
}
