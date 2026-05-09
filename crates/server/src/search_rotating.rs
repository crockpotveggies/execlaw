//! Multi-provider rotating WebSearchApi.
//!
//! Wraps every enabled `config_search_providers` row in a single
//! `WebSearchApi` impl that:
//!
//!   * Round-robins across the enabled providers per call so a
//!     burst of `web_search` tool calls doesn't all land on the
//!     same key.
//!   * Tracks per-provider cooldowns (60s on a 429 / quota / bot-
//!     detection error). A provider in cooldown is skipped on the
//!     next call. After 60s expires it re-enters the rotation.
//!   * Enforces a single global pacing gate (250ms) across ALL
//!     web_search calls regardless of which provider serves them
//!     — smooths the agent's burst-call pattern that triggered
//!     the original quota exhaustion.
//!
//! Cooldown state + rotation cursor live in a process-global
//! `OnceLock<Arc<RotationState>>` so the resolver can keep
//! constructing the wrapper per-call (preserving the existing
//! "config changes apply mid-process without restart" behavior)
//! while the rotation state survives across calls.
//!
//! The wrapper degrades to a single-provider passthrough when
//! only one provider is enabled — no rotation overhead, no
//! global gate (the per-adapter gates already serialize that
//! provider's calls).

use crate::search_rate_limit::RateLimitGate;
use async_trait::async_trait;
use execlaw_core::search_providers::SearchProviderKind;
use execlaw_core::tool::{ApiError, SearchResult, WebSearchApi};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;

/// How long a provider stays on the bench after returning a
/// rate-limit-style error. 60 seconds is short enough that a
/// transient burst clears in under the user's typical attention
/// span, long enough that we don't re-hammer a key whose actual
/// per-minute quota was just exhausted.
const COOLDOWN_DURATION: Duration = Duration::from_secs(60);

/// Minimum elapsed time between any two `web_search` calls
/// regardless of which provider serves them. Conservative — most
/// providers' published per-second limits are higher, but the
/// agent often emits 4 searches in one tool round and the bursty
/// pattern is itself what trips Brave's free tier (1 qps).
const GLOBAL_GAP: Duration = Duration::from_millis(250);

/// Process-global rotation state. Survives across resolver calls
/// so the round-robin cursor + cooldowns aren't reset every time
/// the agent fires a tool call.
pub(crate) struct RotationState {
    /// Per-kind "available again at" timestamps. Missing entry =
    /// not in cooldown. Std mutex (not tokio) so `provider_id` and
    /// other sync paths can access without an `.await`.
    cooldowns: StdMutex<HashMap<SearchProviderKind, Instant>>,
    /// Round-robin cursor. Bumped before each call so the next
    /// invocation starts at a different provider.
    cursor: TokioMutex<usize>,
    /// Global pacing gate across ALL web_search dispatches.
    gate: RateLimitGate,
}

impl RotationState {
    pub(crate) fn fresh(global_gap: Duration) -> Arc<Self> {
        Arc::new(RotationState {
            cooldowns: StdMutex::new(HashMap::new()),
            cursor: TokioMutex::new(0),
            gate: RateLimitGate::new(global_gap),
        })
    }

    fn global() -> Arc<Self> {
        static STATE: OnceLock<Arc<RotationState>> = OnceLock::new();
        STATE
            .get_or_init(|| RotationState::fresh(GLOBAL_GAP))
            .clone()
    }
}

/// Multi-provider rotating wrapper. Construct per-call from the
/// resolver — the providers list reflects the current DB state,
/// the rotation/cooldown state is shared via a process-global.
pub struct RotatingWebSearchApi {
    providers: Vec<(SearchProviderKind, Arc<dyn WebSearchApi>)>,
    state: Arc<RotationState>,
}

impl RotatingWebSearchApi {
    pub fn new(providers: Vec<(SearchProviderKind, Arc<dyn WebSearchApi>)>) -> Self {
        Self {
            providers,
            state: RotationState::global(),
        }
    }

    /// Test-only constructor: build a wrapper with isolated rotation
    /// state instead of the process-global. Each test gets its own
    /// cursor + cooldowns so they don't poison each other.
    #[cfg(test)]
    pub(crate) fn with_state(
        providers: Vec<(SearchProviderKind, Arc<dyn WebSearchApi>)>,
        state: Arc<RotationState>,
    ) -> Self {
        Self { providers, state }
    }
}

#[async_trait]
impl WebSearchApi for RotatingWebSearchApi {
    async fn search(&self, query: &str, max_results: u32) -> Result<Vec<SearchResult>, ApiError> {
        if self.providers.is_empty() {
            return Err(ApiError::Storage(
                "no enabled search providers — visit Settings → Search to enable one".into(),
            ));
        }

        // Global pacing — even if rotation picks a different
        // provider per call, slow the overall web_search rate.
        self.state.gate.wait().await;

        let n = self.providers.len();
        let start = {
            let mut cursor = self.state.cursor.lock().await;
            let v = *cursor;
            *cursor = (v + 1) % n;
            v
        };

        let now = Instant::now();
        let mut last_err: Option<ApiError> = None;
        let mut tried_any = false;

        for offset in 0..n {
            let i = (start + offset) % n;
            let (kind, provider) = &self.providers[i];

            // Skip if in cooldown.
            let in_cooldown = match self.state.cooldowns.lock() {
                Ok(map) => map
                    .get(kind)
                    .copied()
                    .map(|until| until > now)
                    .unwrap_or(false),
                Err(_) => false, // poisoned — best-effort, treat as not cooling
            };
            if in_cooldown {
                continue;
            }
            tried_any = true;

            tracing::debug!(
                target: "search::rotation",
                provider = kind.as_str(),
                start_cursor = start,
                attempt_index = i,
                "dispatching web_search",
            );

            match provider.search(query, max_results).await {
                Ok(results) => {
                    tracing::info!(
                        target: "search::rotation",
                        provider = kind.as_str(),
                        result_count = results.len(),
                        query_len = query.len(),
                        "web_search ok",
                    );
                    return Ok(results);
                }
                Err(e) => {
                    let cooldown_now = is_cooldown_worthy(&e);
                    if cooldown_now {
                        if let Ok(mut map) = self.state.cooldowns.lock() {
                            map.insert(*kind, now + COOLDOWN_DURATION);
                        }
                        tracing::warn!(
                            target: "search::rotation",
                            provider = kind.as_str(),
                            error = %e,
                            cooldown_secs = COOLDOWN_DURATION.as_secs(),
                            "provider rate-limited / quota-exhausted; cooling down + trying next",
                        );
                    } else {
                        tracing::warn!(
                            target: "search::rotation",
                            provider = kind.as_str(),
                            error = %e,
                            "provider failed (non-cooldown); trying next",
                        );
                    }
                    last_err = Some(e);
                }
            }
        }

        // All providers either in cooldown or errored.
        let msg = if !tried_any {
            "every search provider is in cooldown — wait ~60s and retry, or visit Settings → Search to add another"
                .to_owned()
        } else {
            match &last_err {
                Some(e) => format!("all enabled search providers failed; last error: {e}"),
                None => "all search providers failed without surfacing an error".to_owned(),
            }
        };
        Err(ApiError::Storage(msg))
    }

    fn provider_id(&self) -> &str {
        // The actual serving provider varies per call; the
        // rotation log lines record which one answered. Returning
        // a stable string here keeps the tool catalog + tool-result
        // shape consistent across calls.
        "rotating"
    }
}

/// True iff the error message looks like a rate-limit / quota /
/// bot-detection signal that should park the provider for a bit.
/// We pattern-match on substrings because the underlying adapters
/// surface these as `ApiError::Storage(msg)` — there's no
/// structured `RateLimited` variant yet. Substrings chosen from
/// the actual error sites: see `tool_apis_search.rs:336,363` and
/// `tool_apis_search_brave.rs:124`.
fn is_cooldown_worthy(e: &ApiError) -> bool {
    let s = format!("{e}").to_ascii_lowercase();
    s.contains("429")
        || s.contains("rate-limit")
        || s.contains("rate limit")
        || s.contains("quota")
        || s.contains("anomaly")
        || s.contains("bot-detection")
        || s.contains("too many requests")
        || s.contains("key invalid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use execlaw_core::tool::{ApiError, SearchResult};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Test helper — tracks call count, optionally returns a
    /// configured error, optionally returns canned results.
    struct StubProvider {
        id: &'static str,
        calls: AtomicUsize,
        err: Option<ApiError>,
    }

    impl StubProvider {
        fn ok(id: &'static str) -> Self {
            Self {
                id,
                calls: AtomicUsize::new(0),
                err: None,
            }
        }
        fn fail(id: &'static str, err: ApiError) -> Self {
            Self {
                id,
                calls: AtomicUsize::new(0),
                err: Some(err),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl WebSearchApi for StubProvider {
        async fn search(
            &self,
            _query: &str,
            _max_results: u32,
        ) -> Result<Vec<SearchResult>, ApiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(e) = &self.err {
                return Err(e.clone());
            }
            Ok(vec![SearchResult {
                title: format!("ok from {}", self.id),
                url: format!("https://example.com/{}", self.id),
                snippet: None,
            }])
        }
        fn provider_id(&self) -> &str {
            self.id
        }
    }

    /// Each test gets a fresh, isolated RotationState so cursor +
    /// cooldowns from one test don't poison the next. We use a
    /// zero-duration global gate to keep the suite fast (the gate
    /// is exercised in `search_rate_limit::tests`).
    fn fresh_state() -> Arc<RotationState> {
        RotationState::fresh(Duration::ZERO)
    }

    #[tokio::test]
    async fn empty_provider_list_surfaces_clear_error() {
        let rot = RotatingWebSearchApi::with_state(vec![], fresh_state());
        let err = rot.search("hi", 5).await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no enabled search providers"));
    }

    #[tokio::test]
    async fn single_provider_serves_call() {
        let p = Arc::new(StubProvider::ok("ddg"));
        let rot = RotatingWebSearchApi::with_state(
            vec![(SearchProviderKind::DuckDuckGo, p.clone())],
            fresh_state(),
        );
        let results = rot.search("hi", 5).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(p.call_count(), 1);
    }

    #[tokio::test]
    async fn round_robin_distributes_two_calls_across_two_providers() {
        let a = Arc::new(StubProvider::ok("a"));
        let b = Arc::new(StubProvider::ok("b"));
        let rot = RotatingWebSearchApi::with_state(
            vec![
                (SearchProviderKind::DuckDuckGo, a.clone()),
                (SearchProviderKind::Brave, b.clone()),
            ],
            fresh_state(),
        );
        rot.search("hi", 5).await.unwrap();
        rot.search("hi", 5).await.unwrap();
        assert_eq!(
            (a.call_count(), b.call_count()),
            (1, 1),
            "round-robin should distribute one call per provider across two calls",
        );
    }

    #[tokio::test]
    async fn rate_limited_provider_is_skipped_on_next_call() {
        let a = Arc::new(StubProvider::fail(
            "a",
            ApiError::Storage("HTTP 429 (rate-limit)".into()),
        ));
        let b = Arc::new(StubProvider::ok("b"));
        let rot = RotatingWebSearchApi::with_state(
            vec![
                (SearchProviderKind::DuckDuckGo, a.clone()),
                (SearchProviderKind::Brave, b.clone()),
            ],
            fresh_state(),
        );
        // First call: A trips (cursor starts at 0 = A), falls through to B.
        let r = rot.search("hi", 5).await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(a.call_count(), 1);
        assert_eq!(b.call_count(), 1);
        // Second call: A is in cooldown, must be skipped — B serves
        // again. A's call count must not advance.
        rot.search("hi", 5).await.unwrap();
        assert_eq!(
            a.call_count(),
            1,
            "rate-limited provider should be skipped while in cooldown",
        );
        assert_eq!(b.call_count(), 2);
    }

    #[tokio::test]
    async fn non_rate_limit_error_does_not_cooldown() {
        // A always fails with a non-cooldown error; B is healthy.
        // Across 3 calls the cursor cycles 0→1→0, so A is the
        // start-of-rotation on calls 1 + 3. If A's failure
        // tripped the cooldown classifier it would only be hit
        // once (then skipped); since validation errors do NOT
        // cooldown, A must be tried both times.
        let a = Arc::new(StubProvider::fail(
            "exa-bad",
            ApiError::Validation("bad query".into()),
        ));
        let b = Arc::new(StubProvider::ok("ddg-good"));
        let rot = RotatingWebSearchApi::with_state(
            vec![
                (SearchProviderKind::Exa, a.clone()),
                (SearchProviderKind::DuckDuckGo, b.clone()),
            ],
            fresh_state(),
        );
        rot.search("hi", 5).await.unwrap();
        rot.search("hi", 5).await.unwrap();
        rot.search("hi", 5).await.unwrap();
        assert_eq!(
            a.call_count(),
            2,
            "A should be re-tried on call 3 (cursor wrapped back); \
             validation errors must not cooldown",
        );
    }

    #[tokio::test]
    async fn all_failing_returns_aggregated_error() {
        let a = Arc::new(StubProvider::fail(
            "a",
            ApiError::Storage("HTTP 429".into()),
        ));
        let b = Arc::new(StubProvider::fail(
            "b",
            ApiError::Storage("HTTP 429".into()),
        ));
        let rot = RotatingWebSearchApi::with_state(
            vec![
                (SearchProviderKind::DuckDuckGo, a),
                (SearchProviderKind::Brave, b),
            ],
            fresh_state(),
        );
        let err = rot.search("hi", 5).await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("cooldown") || msg.contains("all"),
            "aggregated error must mention cooldown / failure: {msg}",
        );
    }

    #[test]
    fn cooldown_classifier_recognises_known_signals() {
        for s in [
            "HTTP 429",
            "rate-limited",
            "quota exhausted",
            "DDG anomaly page",
            "bot-detection page",
            "Too Many Requests",
            "key invalid",
        ] {
            assert!(
                is_cooldown_worthy(&ApiError::Storage(s.into())),
                "should treat {s:?} as cooldown-worthy",
            );
        }
        for s in ["bad query", "not found", "url malformed"] {
            assert!(
                !is_cooldown_worthy(&ApiError::Validation(s.into())),
                "should NOT treat {s:?} as cooldown-worthy",
            );
        }
    }
}
