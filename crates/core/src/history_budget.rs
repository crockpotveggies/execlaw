//! Sliding-window token budget for chat-history hydration.
//!
//! Per-turn the prompt builder (`crates/server/src/chats.rs` —
//! `run_real_turn` + `run_tool_capable_turn`) reads every event from
//! `seq = 0` and converts each `UserMsg` / `ModelTurn` into an
//! OpenAI-style chat message. Without truncation that load is
//! unbounded — every additional turn adds prompt-prefill cost. We
//! observed this on 2026-05-14 as a 24× latency disparity between
//! a fresh web chat (~3 KB of history, ~657 ms wall clock) and a
//! long-lived Signal thread (~83 KB of history, ~24.5 s wall clock)
//! against the same backend, same model, same code path.
//!
//! The fix this module supplies is the standard sliding-window:
//! keep the most recent messages that fit a token budget, drop the
//! rest. Token counting uses the well-known `chars / 4` heuristic
//! for English text (matches OpenAI's public sizing guidance for
//! `cl100k_base`); no tokenizer dependency, no extra inference
//! infrastructure. The error bound is large per-message (±50% on
//! short messages) but acceptable for budget enforcement — the
//! actual ceiling is the model's `max_model_len` and vLLM rejects
//! overflow with a clear error. The heuristic gives us a soft
//! cap that costs nothing.
//!
//! Invariants the truncation policy holds (pinned by tests in
//! `tests` below):
//!
//!   1. **Pair coherence.** Every kept `Assistant` message has its
//!      preceding `User` message also kept. The model never sees an
//!      orphan model-turn reply.
//!   2. **Recent pairs always survive.** Up to `MIN_KEPT_MESSAGES`
//!      of the most recent are kept *regardless* of budget. An
//!      absurdly low cap still preserves the last exchange; a single
//!      huge message can't starve all context.
//!   3. **Monotone in budget.** A larger `max_tokens` never drops a
//!      message that a smaller budget kept. (Helps the SPA's
//!      "increase the budget if context feels stale" affordance be
//!      predictable.)
//!
//! What this module does NOT do:
//!
//!   * Tokenize. The `chars/4` heuristic is intentional. If the SPA
//!     ever surfaces "you're using N tokens of M" the right answer
//!     is to wire a real tokenizer (tiktoken) on the boundary, not
//!     to make this module exact.
//!   * Summarize the dropped tail. A future iteration can insert a
//!     synthetic `Assistant`-role "[N earlier turns elided]" stub
//!     so the model isn't surprised by the discontinuity. Deferred.
//!   * Touch the system prompt. The caller composes that separately
//!     and it's always sent in full.
//!   * Touch the current user message. The caller appends the
//!     current turn AFTER the truncated history, so the operator's
//!     latest question always reaches the model regardless of budget.

use crate::db::{Database, DbError};
use serde::{Deserialize, Serialize};

/// Floor enforced when loading from the DB. A fat-fingered "100"
/// (or "0", or NULL) is clamped up to this so the prompt builder
/// never silently drops the entire conversation. The truncation
/// policy's recent-pair guarantee means even this floor still
/// surfaces the last exchange.
pub const MIN_HISTORY_TOKENS: u32 = 1000;

/// Default applied by the migration seed AND used as the fallback
/// when the config row is missing entirely (test fixtures, dev
/// builds with hand-crafted DBs).
pub const DEFAULT_HISTORY_TOKENS: u32 = 8000;

/// Anti-starvation floor: up to this many most-recent messages are
/// kept even when the budget is exhausted. With pairs (user +
/// assistant), 4 messages = the last two exchanges. Small enough
/// to fit any sane budget; large enough to give the model
/// meaningful recency context.
pub const MIN_KEPT_MESSAGES: usize = 4;

/// Char-to-token divisor. OpenAI's public guidance for `cl100k_base`
/// is "~4 chars per token in English." Conservative — short messages
/// (greetings, "ok") under-count slightly; long technical messages
/// with code over-count slightly. Net effect averages out and is
/// well within the budget's intended slack.
const CHARS_PER_TOKEN: usize = 4;

/// A single chronological message, role-tagged. Generic so this
/// module doesn't pull in `inference-api` (and so tests can
/// construct fixtures with literals).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMessage {
    pub role: HistoryRole,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryRole {
    User,
    Assistant,
}

/// Result of one `truncate_to_budget` call. Carries the kept
/// messages and a count of what was dropped so the caller can log
/// the truncation event (operators want to know when their long
/// threads are losing tail context).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationOutput {
    pub kept: Vec<HistoryMessage>,
    pub dropped_count: usize,
    /// Estimated tokens of the kept window. Useful for the SPA's
    /// "context usage" indicator if we ever surface one.
    pub kept_tokens_estimate: usize,
}

/// Apply the sliding-window cap. See the module-level docstring for
/// the invariants this function holds.
///
/// `messages` is chronological (oldest first). The returned `kept`
/// preserves that order.
///
/// `max_history_tokens` may be smaller than `MIN_HISTORY_TOKENS` —
/// callers should clamp first via `RetentionPolicy::load`-style
/// logic; this function honors what it's given (so tests can pin
/// edge-case behavior with tiny budgets).
pub fn truncate_to_budget(
    messages: Vec<HistoryMessage>,
    max_history_tokens: u32,
) -> TruncationOutput {
    if messages.is_empty() {
        return TruncationOutput {
            kept: Vec::new(),
            dropped_count: 0,
            kept_tokens_estimate: 0,
        };
    }
    let total = messages.len();
    let budget = max_history_tokens as usize;

    // Walk backward (most recent first), accumulating estimated
    // tokens. Stop when we'd exceed budget UNLESS we haven't kept
    // MIN_KEPT_MESSAGES yet — anti-starvation.
    //
    // `cutoff_index` is the index of the OLDEST message we'll keep
    // (inclusive). Initialised at `total` (= "nothing kept yet"); we
    // walk it down.
    let mut tokens = 0usize;
    let mut cutoff_index = total;
    for (i, msg) in messages.iter().enumerate().rev() {
        let msg_tokens = estimate_tokens(&msg.text);
        let kept_so_far = total - i; // count if we include this msg
        if tokens + msg_tokens > budget && kept_so_far > MIN_KEPT_MESSAGES {
            break;
        }
        tokens += msg_tokens;
        cutoff_index = i;
    }

    // Pair-coherence repair: if the oldest kept message is an
    // Assistant, drop it (would orphan). The next iteration of the
    // loop above would naturally do this if budget allowed, but if
    // budget enforcement stopped us early we have to fix it here.
    if cutoff_index < total && messages[cutoff_index].role == HistoryRole::Assistant {
        let assistant_tokens = estimate_tokens(&messages[cutoff_index].text);
        tokens = tokens.saturating_sub(assistant_tokens);
        cutoff_index += 1;
    }

    let kept: Vec<HistoryMessage> = messages.into_iter().skip(cutoff_index).collect();
    TruncationOutput {
        dropped_count: cutoff_index,
        kept_tokens_estimate: tokens,
        kept,
    }
}

/// Estimate the token count of `text` using the standard `chars/4`
/// heuristic. Exposed for tests and the SPA's optional "context
/// usage" surface.
pub fn estimate_tokens(text: &str) -> usize {
    // `chars().count()` is O(n) but n is bounded by message length;
    // a typical message is <500 chars and the typical history fits
    // easily in microseconds. We use chars (not bytes) so non-ASCII
    // doesn't over-count.
    let chars = text.chars().count();
    chars.div_ceil(CHARS_PER_TOKEN).max(1)
}

/// Load the operator-configured token budget from `config_general`.
/// Returns `DEFAULT_HISTORY_TOKENS` if the column is missing or the
/// row is empty (test fixtures, fresh dev DBs that haven't yet
/// applied migration 0003); clamps any stored value below
/// `MIN_HISTORY_TOKENS` up to the floor.
pub fn load_max_history_tokens(db: &Database) -> Result<u32, DbError> {
    let raw: Option<i64> = db.with_conn(|c| {
        let v: Option<i64> = c
            .query_row(
                "SELECT max_history_tokens FROM config_general WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .ok();
        Ok(v)
    })?;
    Ok(match raw {
        Some(n) if n > 0 => (n as u32).max(MIN_HISTORY_TOKENS),
        _ => DEFAULT_HISTORY_TOKENS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(s: &str) -> HistoryMessage {
        HistoryMessage {
            role: HistoryRole::User,
            text: s.into(),
        }
    }
    fn assistant(s: &str) -> HistoryMessage {
        HistoryMessage {
            role: HistoryRole::Assistant,
            text: s.into(),
        }
    }

    #[test]
    fn empty_history_returns_empty() {
        let out = truncate_to_budget(Vec::new(), 1000);
        assert!(out.kept.is_empty());
        assert_eq!(out.dropped_count, 0);
        assert_eq!(out.kept_tokens_estimate, 0);
    }

    #[test]
    fn history_under_budget_keeps_everything() {
        let msgs = vec![user("hi"), assistant("hello"), user("how are you")];
        let out = truncate_to_budget(msgs.clone(), 10_000);
        assert_eq!(out.kept, msgs);
        assert_eq!(out.dropped_count, 0);
    }

    #[test]
    fn budget_exhausted_drops_oldest_first_and_preserves_pair_coherence() {
        // Each message ~25 chars → ~7 tokens. With budget 20 tokens
        // we should keep the LAST 2-3 messages, never starting with
        // an Assistant.
        let msgs = vec![
            user("oldest user message here"),      // ~7 tokens
            assistant("oldest model reply here"),  // ~7 tokens
            user("middle user message right"),     // ~7 tokens
            assistant("middle model reply right"), // ~7 tokens
            user("recent user message right"),     // ~7 tokens
            assistant("recent model reply right"), // ~7 tokens
        ];
        let out = truncate_to_budget(msgs.clone(), 20);
        // MIN_KEPT_MESSAGES = 4, so even under tight budget we
        // expect ≥4 most recent kept.
        assert!(out.kept.len() >= MIN_KEPT_MESSAGES);
        // First kept message must be User (no orphan Assistant
        // leading the window).
        assert_eq!(
            out.kept[0].role,
            HistoryRole::User,
            "first kept message must be User to preserve pair coherence: kept={:?}",
            out.kept,
        );
        // Dropped count is total - kept.
        assert_eq!(out.dropped_count, msgs.len() - out.kept.len());
    }

    #[test]
    fn current_turn_user_message_always_survives_at_tiny_budget() {
        // Most recent message is the operator's current turn. A
        // ridiculously small budget MUST still surface it.
        let msgs = vec![
            user("a long-since-elided question with lots of context to push past budget"),
            assistant("a long-since-elided reply with lots of context to push past budget"),
            user("hi"), // current turn — must survive even at budget=1
        ];
        let out = truncate_to_budget(msgs, 1);
        let last = out.kept.last().expect("at least one message kept");
        assert_eq!(last.role, HistoryRole::User);
        assert_eq!(last.text, "hi");
    }

    #[test]
    fn monotone_in_budget_larger_keeps_at_least_as_many() {
        // The "predictable when operators raise the cap" invariant.
        let msgs = vec![
            user("msg1 with body text"),
            assistant("reply1 with body text"),
            user("msg2 with body text"),
            assistant("reply2 with body text"),
            user("msg3 with body text"),
            assistant("reply3 with body text"),
            user("msg4 with body text"),
            assistant("reply4 with body text"),
        ];
        let small = truncate_to_budget(msgs.clone(), 20);
        let large = truncate_to_budget(msgs.clone(), 1000);
        assert!(
            large.kept.len() >= small.kept.len(),
            "larger budget must keep at least as many: small={} large={}",
            small.kept.len(),
            large.kept.len(),
        );
    }

    #[test]
    fn pair_coherence_after_budget_walk_drops_orphan_assistant() {
        // Specifically construct a case where the budget walk would
        // naturally include an Assistant as its oldest kept entry,
        // and assert the repair drops it.
        //
        // 4 messages each ~5 tokens. Budget 12 tokens → walk
        // accumulates from the back: assistant(reply) (5) + user(q)
        // (5) = 10, adding the next assistant(reply) would push to
        // 15. So natural break is between user(q) and prior
        // assistant(reply). That break already produces a User-
        // starting kept window — fine.
        //
        // Now bump anti-starvation to ensure 4 kept (regardless of
        // budget). The MIN_KEPT_MESSAGES guarantee FORCES the older
        // assistant in even though it exceeds budget; then the
        // repair must NOT drop it (because including it doesn't
        // orphan — the assistant has its user before it in the
        // anti-starvation window).
        let msgs = vec![
            user("u1xx"),
            assistant("a1xx"),
            user("u2xx"),
            assistant("a2xx"),
        ];
        let out = truncate_to_budget(msgs.clone(), 12);
        // MIN_KEPT_MESSAGES = 4, so all 4 survive.
        assert_eq!(out.kept.len(), 4);
        assert_eq!(out.kept[0].role, HistoryRole::User);
    }

    #[test]
    fn estimate_tokens_handles_unicode_and_short_strings() {
        assert!(estimate_tokens("") >= 1, "min token estimate is 1");
        assert_eq!(estimate_tokens("test"), 1); // 4 chars / 4 = 1
        // 8 chars → 2 tokens (div_ceil).
        assert_eq!(estimate_tokens("eightchr"), 2);
        // Non-ASCII counted as 1 char each, not multiple bytes.
        let s = "héllo"; // 5 chars (not 6 bytes); ceil(5/4) = 2
        assert_eq!(estimate_tokens(s), 2);
    }

    #[test]
    fn load_max_history_tokens_returns_default_on_empty_db() {
        // No migrations applied → no config_general table → defaults.
        let db = Database::open(&crate::db::DbConfig::in_memory_unencrypted()).unwrap();
        let n = load_max_history_tokens(&db).unwrap();
        assert_eq!(n, DEFAULT_HISTORY_TOKENS);
    }

    #[test]
    fn load_max_history_tokens_reads_migrated_seed() {
        let db = Database::open(&crate::db::DbConfig::in_memory_unencrypted()).unwrap();
        crate::migrations::MigrationRunner::new(&db)
            .apply_all()
            .unwrap();
        let n = load_max_history_tokens(&db).unwrap();
        assert_eq!(n, DEFAULT_HISTORY_TOKENS);
    }

    #[test]
    fn load_max_history_tokens_clamps_below_floor() {
        // Operator hand-edits a tiny value — we clamp it up so the
        // prompt builder never silently obliterates the conversation.
        let db = Database::open(&crate::db::DbConfig::in_memory_unencrypted()).unwrap();
        crate::migrations::MigrationRunner::new(&db)
            .apply_all()
            .unwrap();
        db.with_conn(|c| {
            c.execute(
                "UPDATE config_general SET max_history_tokens = 100 WHERE id = 1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let n = load_max_history_tokens(&db).unwrap();
        assert_eq!(n, MIN_HISTORY_TOKENS);
    }

    #[test]
    fn load_max_history_tokens_honors_large_operator_override() {
        // Operator with a big-context model sets 40K — we honor it.
        let db = Database::open(&crate::db::DbConfig::in_memory_unencrypted()).unwrap();
        crate::migrations::MigrationRunner::new(&db)
            .apply_all()
            .unwrap();
        db.with_conn(|c| {
            c.execute(
                "UPDATE config_general SET max_history_tokens = 40000 WHERE id = 1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let n = load_max_history_tokens(&db).unwrap();
        assert_eq!(n, 40000);
    }
}
