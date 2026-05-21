//! ReplyRouter — turns a [`ReplyPayload`] into delivered output on
//! the right transport, with rich-part degradation per-handler and a
//! fallback ladder when delivery fails (M6).
//!
//! The router is the only code that knows how to translate the
//! `ReplyPayload` enum into transport-specific calls. Flow nodes
//! (`SendReply`, `Notify`) hand a `ReplyPayload` + the run's
//! `EventEnvelope`; the router resolves `envelope.origin` to a
//! `RegisteredReplyHandler` (loaded from the registry), packs each
//! `ReplyPart` per that handler's capability matrix, and emits.
//!
//! Failure semantics (per design doc §4 of docs/automations-event-driven.md):
//!
//! 1. Try the resolved handler with the fully-degraded payload (Tier 1)
//! 2. Drop rich parts that the transport can't handle (Tier 2)
//! 3. Replace attachments with signed URLs in the text body (Tier 3)
//! 4. Send text only (Tier 4)
//! 5. Fall back to `payload.hints.on_failure` — default is
//!    `ChatAppendHome` (post to operator Inbox + fire alert)
//!
//! Each tier failure carries the underlying error forward into the
//! returned `RouteResult` so the trace records WHY a tier failed.
//!
//! Streaming: slice 4 ships the static-only path. Streaming
//! integration (where the router subscribes to the per-run flow
//! channel and dispatches `StreamItem` deltas) lands in slice 6
//! once the `FlowEventSink` plumbing exists.

mod capabilities;
mod degrade;
mod handlers;
mod tiers;

pub use capabilities::Capabilities;

use execlaw_core::Database;
use execlaw_core::event_envelope::{EventEnvelope, OriginRef};
use execlaw_core::event_registry::{EventRegistry, RegisteredReplyHandler};
use execlaw_core::reply::{FailureFallback, ReplyHints, ReplyPayload};
use execlaw_plugin_host::PluginHost;

/// Slim handle the router needs — pulled out of `AppState` so the
/// automation runtime's `ExecutorContext` can hand the router what
/// it owns without taking a runtime dep on the full server state.
#[derive(Clone)]
pub struct RouterCtx {
    pub db: Database,
    pub plugin_host: Option<PluginHost>,
    /// EventBus handle for `UiEvent::*` broadcasts (chat surface
    /// subscribers). `None` in unit tests that don't render
    /// outbound chat messages.
    pub events: Option<crate::events::EventBus>,
    /// HMAC chaining key for `EventLog::commit_turn`. `None` for
    /// pre-setup + tests; production loads from the vault.
    pub event_log_hmac_key: Option<std::sync::Arc<Vec<u8>>>,
}

impl RouterCtx {
    pub fn new(db: Database, plugin_host: Option<PluginHost>) -> Self {
        Self {
            db,
            plugin_host,
            events: None,
            event_log_hmac_key: None,
        }
    }

    pub fn with_events(mut self, events: crate::events::EventBus) -> Self {
        self.events = Some(events);
        self
    }

    pub fn with_event_log_hmac_key(
        mut self,
        key: Option<std::sync::Arc<Vec<u8>>>,
    ) -> Self {
        self.event_log_hmac_key = key;
        self
    }
}

/// Outcome of one routing attempt. Recorded in the flow run's
/// step_trace so the operator can see which tier landed.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RouteResult {
    /// Delivered successfully via Tier N of the degradation ladder.
    Sent {
        tier: &'static str,
        /// Opaque handler-returned identifier (message_id, alert_id,
        /// or "" for ws/drop where there's nothing to track).
        receipt: String,
    },
    /// Primary handler failed but the fallback ladder recovered.
    /// Operator gets the work product via Inbox + alert.
    Recovered {
        tier: &'static str,
        original_err: String,
    },
    /// Explicit `payload.hints.on_failure = Drop`. Nothing delivered.
    DroppedByHint,
    /// Even the fallback failed. Operator sees an alert but the
    /// payload may be lost — the trace records the chain.
    Failed { err: String },
}

/// Entry point for `SendReply` node and direct callers.
///
/// Returns synchronously (no awaiting on transports — those calls
/// happen inside the function, possibly via `tokio::task::block_in_place`
/// for handlers that go over async plugin RPC). Slice 6 adds a
/// `route_streaming()` sibling that drives a stream over the run
/// channel until `StreamItem::Done`.
pub async fn route(
    ctx: &RouterCtx,
    envelope: &EventEnvelope,
    payload: ReplyPayload,
) -> RouteResult {
    // Resolve the handler. `OriginRef::None` short-circuits to
    // DroppedByHint — nothing to deliver to.
    if matches!(envelope.origin, OriginRef::None) {
        return RouteResult::DroppedByHint;
    }

    let caps = match resolve_capabilities(ctx, &envelope.origin) {
        Ok(c) => c,
        Err(err) => {
            // No handler registered for this origin — fall straight
            // to fallback.
            return fallback(ctx, &payload, err).await;
        }
    };

    // Tier loop — descend through degradation tiers until one
    // succeeds or all fail.
    let tiers = tiers::build_tiers(&payload, &caps);
    let mut last_err: Option<String> = None;
    for (tier_label, prepared) in tiers {
        match handlers::send(ctx, &envelope.origin, &caps, &prepared).await {
            Ok(receipt) => {
                return RouteResult::Sent {
                    tier: tier_label,
                    receipt,
                };
            }
            Err(handlers::SendError::Transient(e)) => {
                last_err = Some(e);
                continue;
            }
            Err(handlers::SendError::Permanent(e)) => {
                last_err = Some(e);
                break;
            }
        }
    }
    fallback(
        ctx,
        &payload,
        last_err.unwrap_or_else(|| "all tiers failed without reason".into()),
    )
    .await
}

/// Resolve the registered reply handler for an origin. Built-in
/// handlers (web_socket_session, chat_append, alert, drop) MUST
/// exist in the registry — `cmd_serve` seeds them at boot.
fn resolve_capabilities(
    ctx: &RouterCtx,
    origin: &OriginRef,
) -> Result<Capabilities, String> {
    let name = match origin {
        OriginRef::WebSocketSession { .. } => "web_socket_session",
        OriginRef::PluginChannel { plugin_id, .. } => plugin_id.as_str(),
        OriginRef::ChatAppend { .. } => "chat_append",
        OriginRef::Alert => "alert",
        OriginRef::None => "drop",
    };
    let reg = EventRegistry::new(&ctx.db);
    let handler = reg
        .get_reply_handler(name)
        .map_err(|e| format!("registry lookup failed: {e}"))?
        .ok_or_else(|| {
            format!(
                "no reply handler registered for origin '{}' (registry rows are missing — check `register_core_event_kinds` ran)",
                name
            )
        })?;
    Ok(Capabilities::from_registered(handler))
}

/// Last-resort delivery path. Tries the hint's fallback target. If
/// THAT fails too, fires an alert and returns RouteResult::Failed
/// (operator sees the alert; payload is recorded in the flow trace).
async fn fallback(ctx: &RouterCtx, payload: &ReplyPayload, err: String) -> RouteResult {
    let hint = payload
        .hints
        .on_failure
        .clone()
        .unwrap_or(FailureFallback::ChatAppendHome);

    match hint {
        FailureFallback::Drop => {
            tracing::warn!(error = %err, "reply dropped by FailureFallback::Drop hint");
            RouteResult::DroppedByHint
        }
        FailureFallback::AlertOnly => {
            if let Err(e2) = handlers::fire_delivery_failure_alert(ctx, payload, &err) {
                return RouteResult::Failed {
                    err: format!("primary={err}; alert_fallback={e2}"),
                };
            }
            RouteResult::Recovered {
                tier: "alert_only",
                original_err: err,
            }
        }
        FailureFallback::ChatAppendHome => {
            // Mint/find the operator Inbox, append the payload as a
            // system-authored message, then ALSO fire an alert so the
            // operator notices something failed.
            match handlers::deliver_to_operator_home(ctx, payload, &err).await {
                Ok(_) => {
                    let _ = handlers::fire_delivery_failure_alert(ctx, payload, &err);
                    RouteResult::Recovered {
                        tier: "chat_append_home",
                        original_err: err,
                    }
                }
                Err(home_err) => {
                    // Even Inbox failed — last gasp is the alert.
                    if let Err(alert_err) = handlers::fire_delivery_failure_alert(ctx, payload, &err) {
                        return RouteResult::Failed {
                            err: format!(
                                "primary={err}; home_fallback={home_err}; alert_fallback={alert_err}"
                            ),
                        };
                    }
                    RouteResult::Recovered {
                        tier: "alert_only_after_home_failed",
                        original_err: err,
                    }
                }
            }
        }
    }
}

/// Public helper for callers that have NO envelope context (e.g.,
/// direct admin-triggered "post this to my Inbox"). Wraps the
/// fallback path so the same delivery + alert behavior fires.
pub async fn route_to_inbox(
    ctx: &RouterCtx,
    payload: ReplyPayload,
    reason: &str,
) -> RouteResult {
    // We synthesize a fake "primary err" so the fallback flow
    // attributes the delivery to operator-intent rather than
    // an upstream failure.
    fallback(
        ctx,
        &payload,
        format!("explicit-route-to-inbox: {reason}"),
    )
    .await
}

#[allow(dead_code)] // used in slice 6
pub use degrade::pack_part;

#[allow(dead_code)]
pub fn empty_hints() -> ReplyHints {
    ReplyHints::default()
}

// The unused import warning silencer; `RegisteredReplyHandler` is
// surfaced through Capabilities and is the canonical inbound shape
// for testing.
#[allow(dead_code)]
fn _touch_registered() {
    let _ = std::any::TypeId::of::<RegisteredReplyHandler>;
}
