//! Wakes the agent in a conversation when the deep-research planner
//! needs operator clarification.
//!
//! Subscribes to `EventBus` for `UiEvent::ResearchAwaitingInput` and
//! fires `chats::dispatch_clarification_turn` for each one. The
//! agent receives a system-framed prompt instructing it to relay the
//! planner's question to the user in chat, then call
//! `research_clarify` once the user answers.
//!
//! Why this exists
//! ---------------
//! Pre-rev-7 design: `research_start` would block the agent's tool
//! turn through the planner phase so the agent's tool result already
//! reflected `awaiting_input`, letting it relay the question in the
//! same turn. That worked but coupled the agent's responsiveness to
//! the planner's wall-clock latency (5–15 s typical, occasionally
//! longer on cold model loads). The event-driven path here decouples
//! them: `research_start` returns immediately with a Pending row,
//! the listener wakes the agent only when (and if) the planner
//! actually decides clarification is needed.
//!
//! Idempotency
//! -----------
//! The runner emits exactly one `ResearchAwaitingInput` event per
//! AwaitingInput transition. But a job that is clarified, re-planned,
//! and clarified again will emit a second event (legitimately) —
//! same `job_id`, different question. The cache keys on
//! `(job_id, question)` so the second clarification round wakes the
//! agent again, but a duplicate event for the same question (e.g.
//! from a misbehaving caller publishing twice) is dropped.
//! Cache entries expire after 60 s — long enough to dedupe near-
//! simultaneous duplicates, short enough that operator-time gaps
//! don't accumulate state.

use crate::events::{EventBus, UiEvent};
use crate::state::AppState;
use execlaw_core::ids::ConversationId;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Tunable: how long a `(job_id, question)` pair stays in the
/// dedupe cache. Picked as "longer than any plausible event-bus
/// delivery jitter, shorter than the gap between a user answering
/// one clarification and the planner posing a follow-up".
const DEDUPE_TTL: Duration = Duration::from_secs(60);

/// Spawn the listener as a background task. Returns the join handle
/// so the caller (server boot) can keep the task alive — letting it
/// drop would shut the listener down on the next event.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    let bus = state.events.clone();
    tokio::spawn(async move {
        run(state, bus).await;
    })
}

/// Main loop. Subscribes to the bus, dispatches on each
/// `ResearchAwaitingInput`. Exits cleanly when the bus is dropped
/// (which happens on server shutdown).
pub async fn run(state: AppState, bus: EventBus) {
    let dedupe: Arc<Mutex<HashMap<(String, String), Instant>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut rx = bus.subscribe();
    tracing::info!("clarification_listener: subscribed to event bus");
    loop {
        match rx.recv().await {
            Ok(UiEvent::ResearchAwaitingInput {
                conversation_id,
                job_id,
                question,
            }) => {
                if !should_fire(&dedupe, &job_id, &question).await {
                    tracing::debug!(
                        job_id = %job_id,
                        "clarification_listener: deduping (recent identical event)",
                    );
                    continue;
                }
                let state_for_task = state.clone();
                tokio::spawn(async move {
                    let cid = ConversationId::from(conversation_id.as_str());
                    match crate::chats::dispatch_clarification_turn(
                        &state_for_task,
                        &cid,
                        &job_id,
                        &question,
                    )
                    .await
                    {
                        Ok(out) => {
                            tracing::info!(
                                job_id = %job_id,
                                conversation_id = %conversation_id,
                                reply_chars = out.assistant_text.chars().count(),
                                "clarification_listener: agent relayed question to user",
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                job_id = %job_id,
                                conversation_id = %conversation_id,
                                error = %e,
                                "clarification_listener: dispatch failed; \
                                 user will see only the awaiting-input card and \
                                 not get an agent message",
                            );
                        }
                    }
                });
            }
            Ok(_) => {
                // Other event kinds — not our business.
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                // Lagged on the broadcast channel. Cheap to recover —
                // ResearchAwaitingInput is rare, so the missed events
                // are almost always non-clarification chatter (token
                // deltas etc.). Log and continue.
                tracing::warn!(
                    skipped = n,
                    "clarification_listener: lagged on event bus; some events skipped",
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::info!("clarification_listener: event bus closed; exiting");
                return;
            }
        }
    }
}

/// Returns `true` when the (job_id, question) pair has NOT been
/// seen recently — i.e. dispatch should proceed. Updates the cache
/// as a side effect. Also opportunistically prunes expired entries.
async fn should_fire(
    cache: &Arc<Mutex<HashMap<(String, String), Instant>>>,
    job_id: &str,
    question: &str,
) -> bool {
    let now = Instant::now();
    let key = (job_id.to_owned(), question.to_owned());
    let mut guard = cache.lock().await;
    // Prune expired. Cheap (cache stays small in practice — at most
    // a handful of in-flight research jobs per conversation).
    guard.retain(|_, &mut ts| now.duration_since(ts) < DEDUPE_TTL);
    if guard.contains_key(&key) {
        return false;
    }
    guard.insert(key, now);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_fire_first_time_then_dedupes_until_ttl_or_question_changes() {
        let cache: Arc<Mutex<HashMap<(String, String), Instant>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // First call — fresh — must fire.
        assert!(should_fire(&cache, "job-1", "Which zone?").await);
        // Same key — must dedupe.
        assert!(!should_fire(&cache, "job-1", "Which zone?").await);
        // Different question on same job — different round, must fire.
        assert!(should_fire(&cache, "job-1", "Which light level?").await);
        // Different job, same question text — different job, must fire.
        assert!(should_fire(&cache, "job-2", "Which zone?").await);
    }

    #[tokio::test]
    async fn should_fire_clears_after_ttl_expires() {
        // Populate the cache, then mutate the entry's timestamp to
        // be older than the TTL so the prune step removes it. Avoids
        // sleeping for 60 s in the test.
        let cache: Arc<Mutex<HashMap<(String, String), Instant>>> =
            Arc::new(Mutex::new(HashMap::new()));
        assert!(should_fire(&cache, "j", "q").await);
        {
            let mut g = cache.lock().await;
            // Force the entry to look expired.
            let stale = Instant::now() - DEDUPE_TTL - Duration::from_secs(1);
            g.insert(("j".into(), "q".into()), stale);
        }
        // The next call's prune pass should evict, then fresh fire.
        assert!(should_fire(&cache, "j", "q").await);
    }
}
