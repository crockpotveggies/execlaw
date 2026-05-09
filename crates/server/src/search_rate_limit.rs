//! Shared per-process rate-limit gate for search-provider adapters.
//!
//! Background: every search-provider adapter we ship is hit by the
//! gather phase's parallel-worker pool (default 3). Without
//! serialization, three sub-queries' searches fire within ms — and
//! providers like Brave Search (free tier: 1 query/sec) bounce
//! all-but-one with HTTP 429. Operator-visible result was "4/5
//! sections failed" on a 5-step gather plan.
//!
//! Each provider gets its OWN `RateLimitGate` (DDG had this baked
//! into `DuckDuckGoSearchApi`; this module is the generalisation).
//! Different providers have different per-second ceilings; the gate
//! takes a `Duration` at construction so the per-adapter
//! configuration stays local to that adapter's module.
//!
//! Lock semantics: a single `Mutex<Option<Instant>>` recording the
//! last call's start time. The gate sleeps to the next allowed
//! slot, then bumps the timestamp to *now* on exit so the next
//! caller serializes against this call's wall-clock. The lock is
//! held only across the gap-check + sleep — never across the actual
//! HTTP call — so contention is bounded by the configured gap and
//! tasks that don't need rate-limiting (other adapters) aren't
//! affected.
//!
//! 2026-05-04.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Per-process rate-limit gate. Cheap to construct (one Arc +
/// Mutex); construct one per provider instance and call
/// `.wait().await` before every outbound request.
#[derive(Clone)]
pub struct RateLimitGate {
    last: Arc<Mutex<Option<Instant>>>,
    min_gap: Duration,
}

impl RateLimitGate {
    /// Build a gate with the given minimum gap between consecutive
    /// requests. `Duration::ZERO` is a no-op gate (useful for tests
    /// or for providers without a published rate limit).
    pub fn new(min_gap: Duration) -> Self {
        Self {
            last: Arc::new(Mutex::new(None)),
            min_gap,
        }
    }

    /// Wait until at least `min_gap` has elapsed since the previous
    /// call's start. Bumps the timestamp to *now* on exit.
    pub async fn wait(&self) {
        if self.min_gap.is_zero() {
            return;
        }
        let mut guard = self.last.lock().await;
        if let Some(prev) = *guard {
            let elapsed = prev.elapsed();
            if elapsed < self.min_gap {
                tokio::time::sleep(self.min_gap - elapsed).await;
            }
        }
        *guard = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn second_call_waits_until_min_gap_has_elapsed() {
        // Use a small gap so the test runs fast (60 ms total)
        // without relying on the tokio paused-clock feature
        // (which requires `tokio/test-util` and isn't enabled
        // workspace-wide).
        let gate = RateLimitGate::new(Duration::from_millis(60));
        let start = Instant::now();
        gate.wait().await; // first call — no wait
        let after_first = start.elapsed();
        assert!(after_first < Duration::from_millis(20));

        gate.wait().await; // second call — must sleep ~60 ms
        let after_second = start.elapsed();
        assert!(
            after_second >= Duration::from_millis(60),
            "second call should have waited ≥ 60 ms; observed {after_second:?}",
        );
        // Loose upper bound — a wildly over-sleeping gate would
        // also be a bug.
        assert!(
            after_second < Duration::from_millis(500),
            "second call shouldn't oversleep; observed {after_second:?}",
        );
    }

    #[tokio::test]
    async fn zero_min_gap_is_a_no_op_gate() {
        let gate = RateLimitGate::new(Duration::ZERO);
        let start = Instant::now();
        gate.wait().await;
        gate.wait().await;
        gate.wait().await;
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn third_call_waits_relative_to_the_second_not_the_first() {
        // Catches the regression where the gate stamps the FIRST
        // call's time only — without per-call bumping, a long
        // burst would all fire after the first sleep.
        let gate = RateLimitGate::new(Duration::from_millis(50));
        let start = Instant::now();
        gate.wait().await;
        gate.wait().await;
        gate.wait().await;
        // Expected: 0 + 50 + 50 = 100 ms total
        let total = start.elapsed();
        assert!(
            total >= Duration::from_millis(100),
            "three consecutive calls should sleep ≥ 100 ms; observed {total:?}",
        );
    }
}
