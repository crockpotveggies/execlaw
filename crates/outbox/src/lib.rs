//! execlaw-outbox
//!
//! Effect relay — drains `state_outbox`, dispatches effects to registered
//! consumers, honors framework-minted idempotency keys and exponential
//! backoff (§2.4, §2.15).
//!
//! Phase 0 ships the backoff helper + retry-budget type so the main relay
//! loop (Phase 1) just needs the dispatch wiring.

#![forbid(unsafe_code)]

use std::time::Duration;

/// How long to wait before retrying a failed effect.
///
/// Baseline: 1s, doubled per attempt, capped at 10 minutes. After
/// `max_attempts`, the caller should move the row to `dead_letter` and
/// fire an Error alert.
pub fn exp_backoff(attempt: u32) -> Duration {
    const BASE_MS: u64 = 1_000;
    const CAP_MS: u64 = 10 * 60 * 1_000;
    let ms = BASE_MS.saturating_mul(1u64.checked_shl(attempt.min(15)).unwrap_or(1));
    Duration::from_millis(ms.min(CAP_MS))
}

#[derive(Debug, Clone, Copy)]
pub struct RetryBudget {
    pub max_attempts: u32,
}

impl Default for RetryBudget {
    fn default() -> Self {
        // §2.15: "Per-effect retry budget: 5 attempts with exponential
        // backoff to a ceiling, then move to a dead_letter table."
        Self { max_attempts: 5 }
    }
}

impl RetryBudget {
    pub fn should_dead_letter(&self, attempts: u32) -> bool {
        attempts >= self.max_attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let a0 = exp_backoff(0);
        let a1 = exp_backoff(1);
        let a2 = exp_backoff(2);
        let big = exp_backoff(20);
        assert!(a1 > a0);
        assert!(a2 > a1);
        assert_eq!(big, Duration::from_millis(10 * 60 * 1_000));
    }

    #[test]
    fn default_budget_is_five() {
        let b = RetryBudget::default();
        assert_eq!(b.max_attempts, 5);
        assert!(!b.should_dead_letter(4));
        assert!(b.should_dead_letter(5));
        assert!(b.should_dead_letter(6));
    }
}
