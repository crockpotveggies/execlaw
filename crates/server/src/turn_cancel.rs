//! Per-turn cancellation tokens.
//!
//! 2026-04-28 — added so the SPA can hit `POST /api/chats/:id/stop` and
//! halt an in-flight turn mid-stream. Without this, a runaway local
//! model could happily generate for minutes before returning, with no
//! escape hatch beyond killing the server. The streaming path
//! (`chats::run_real_turn`) checks the flag on every SSE chunk and
//! short-circuits when set; the tool-capable path checks at tool-round
//! boundaries.
//!
//! Design notes:
//!   * Keyed by conversation_id rather than turn_seq. The SPA never
//!     learns the seq before the turn is in flight, so addressing a
//!     stop at "the conversation's current turn, whatever it is" is
//!     the natural granularity.
//!   * A turn that completes normally calls `disarm` via the RAII
//!     guard in `send_message`, removing the entry. A turn that's
//!     stopped mid-flight observes `is_cancelled() == true` and exits
//!     its loop early; the entry is then removed by the same RAII
//!     guard in the same `send_message` frame. The stop endpoint
//!     itself never removes entries — only the turn owner does — so a
//!     burst of stop requests during a single turn is idempotent.
//!   * `Arc<AtomicBool>` rather than `Notify` because the consumer
//!     polls between SSE chunks anyway (the inference HTTP client
//!     yields one chunk at a time on a tokio task that already wakes
//!     on the network read). A signal-style channel would be more
//!     elegant if the runner had to interrupt a *blocking* read, but
//!     reqwest's body stream is already async, so a plain flag check
//!     between chunks is sufficient.

use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct TurnCancellationRegistry {
    inner: Arc<DashMap<String, Arc<AtomicBool>>>,
}

impl TurnCancellationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a fresh cancel flag for `conversation_id`. Replaces any
    /// existing entry — the previous turn's flag is dropped, but its
    /// owner already cleaned it up via [`Self::clear`] on the success
    /// path, so the only realistic source of an existing entry is a
    /// stuck turn whose owner panicked. Replacing it lets the new turn
    /// run; the orphaned `AtomicBool` is harmless.
    pub fn register(&self, conversation_id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.inner.insert(conversation_id.to_owned(), flag.clone());
        flag
    }

    /// Set the cancel flag for `conversation_id`. Returns `true` if a
    /// turn was actually in flight (entry existed); `false` if the
    /// stop request beat or trailed the turn. Either case is fine for
    /// the SPA — the response is the same — but we surface the bool
    /// so callers can log "stop arrived after turn finished" without
    /// alarming the operator.
    pub fn cancel(&self, conversation_id: &str) -> bool {
        if let Some(entry) = self.inner.get(conversation_id) {
            entry.value().store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// Remove the flag for `conversation_id`. Called by the turn owner
    /// (success and error paths both) via the [`TurnCancelGuard`]
    /// RAII. Idempotent — multiple drops are harmless.
    pub fn clear(&self, conversation_id: &str) {
        self.inner.remove(conversation_id);
    }

    /// Test helper: how many entries are live.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

/// RAII guard: registers a cancel flag on construction, removes it on
/// drop. Lets `send_message` thread the flag into the streaming loop
/// without worrying about cleanup on each early-return arm.
pub struct TurnCancelGuard {
    registry: TurnCancellationRegistry,
    conversation_id: String,
    pub flag: Arc<AtomicBool>,
}

impl TurnCancelGuard {
    pub fn new(registry: TurnCancellationRegistry, conversation_id: String) -> Self {
        let flag = registry.register(&conversation_id);
        Self {
            registry,
            conversation_id,
            flag,
        }
    }

    /// Returns true once the operator (or any other caller) has hit
    /// `POST /api/chats/:id/stop` for this conversation.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

impl Drop for TurnCancelGuard {
    fn drop(&mut self) {
        self.registry.clear(&self.conversation_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_cancel_flips_flag() {
        let reg = TurnCancellationRegistry::new();
        let flag = reg.register("conv-1");
        assert!(!flag.load(Ordering::SeqCst));
        assert!(reg.cancel("conv-1"));
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn cancel_returns_false_when_no_turn_in_flight() {
        let reg = TurnCancellationRegistry::new();
        assert!(!reg.cancel("conv-nonexistent"));
    }

    #[test]
    fn guard_drops_entry_on_scope_exit() {
        let reg = TurnCancellationRegistry::new();
        {
            let _g = TurnCancelGuard::new(reg.clone(), "conv-2".into());
            assert_eq!(reg.len(), 1);
        }
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn guard_observes_external_cancel() {
        let reg = TurnCancellationRegistry::new();
        let g = TurnCancelGuard::new(reg.clone(), "conv-3".into());
        assert!(!g.is_cancelled());
        reg.cancel("conv-3");
        assert!(g.is_cancelled());
    }

    #[test]
    fn second_register_replaces_first() {
        // A new turn on the same conversation should always start with
        // a fresh, unset flag — even if the prior turn's owner failed
        // to clean up. The prior turn's `is_cancelled()` won't
        // observe the new turn's stops, but that's acceptable: the
        // prior owner is presumed dead.
        let reg = TurnCancellationRegistry::new();
        let f1 = reg.register("conv-4");
        f1.store(true, Ordering::SeqCst);
        let f2 = reg.register("conv-4");
        assert!(!f2.load(Ordering::SeqCst));
        // f1 is now orphaned but still observable to its dead owner;
        // the live registry only sees f2.
        reg.cancel("conv-4");
        assert!(f2.load(Ordering::SeqCst));
    }
}
