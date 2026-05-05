//! Brave Search API adapter.
//!
//! Operator-supplied API key (free tier: 2k queries/month at time
//! of writing; paid tiers from $5/1k). The Brave API is purpose-
//! built for AI / agent use — fast, no rate-limit roulette, JSON
//! response format, no scraping. The right escape hatch from DDG
//! when self-hosting SearxNG isn't an option.
//!
//! Wire format (https://api.search.brave.com/app/documentation):
//!
//!   GET https://api.search.brave.com/res/v1/web/search?q=<query>&count=N
//!   Header: X-Subscription-Token: <api_key>
//!
//!   Response: { "web": { "results": [{ "title", "url", "description" }] } }
//!
//! 2026-05-04.

use async_trait::async_trait;
use execlaw_core::tool::{ApiError, SearchResult, WebSearchApi};
use serde::Deserialize;
use std::time::Duration;

const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const DEFAULT_TIMEOUT_S: u64 = 20;

/// Brave free tier is 1 query/sec; the 1100 ms gap leaves a small
/// safety margin so concurrent gather workers don't trip the
/// limiter. Paid plans raise the per-second ceiling but at the
/// cost of a potential burst-bounce on free tier — the conservative
/// default is the right call until per-provider config exposes
/// this knob to the operator.
///
/// Without this gate, `parallel_workers=3` (gather default) burst-
/// fires three searches within ms → Brave HTTP 429 on two of them →
/// "4/5 sections failed" on a 5-step plan. Same shape as the DDG
/// burst issue fixed earlier; same shape of fix.
const BRAVE_MIN_REQUEST_GAP: std::time::Duration = std::time::Duration::from_millis(1_100);

#[derive(Debug, Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
}

#[derive(Debug, Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Debug, Deserialize)]
struct BraveResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    /// Brave calls the snippet `description`. Some result kinds
    /// (videos, news) have a `meta_url` field too but the basic
    /// `description` covers the gather-phase use case.
    #[serde(default)]
    description: String,
}

pub struct BraveSearchApi {
    client: reqwest::Client,
    api_key: String,
    rate_limit: crate::search_rate_limit::RateLimitGate,
}

impl BraveSearchApi {
    pub fn new(api_key: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_S))
            .user_agent(crate::tool_apis_http::DEFAULT_USER_AGENT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            api_key: api_key.into(),
            rate_limit: crate::search_rate_limit::RateLimitGate::new(BRAVE_MIN_REQUEST_GAP),
        }
    }

    pub fn with_client(api_key: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            client,
            api_key: api_key.into(),
            rate_limit: crate::search_rate_limit::RateLimitGate::new(BRAVE_MIN_REQUEST_GAP),
        }
    }
}

#[async_trait]
impl WebSearchApi for BraveSearchApi {
    fn provider_id(&self) -> &str {
        "brave"
    }
    async fn search(&self, query: &str, max_results: u32) -> Result<Vec<SearchResult>, ApiError> {
        if self.api_key.is_empty() {
            return Err(ApiError::Validation(
                "Brave api_key is empty; configure it in Settings → Search".into(),
            ));
        }
        // Serialize concurrent searches behind the per-process gate
        // so a burst of parallel gather workers doesn't trip Brave's
        // free-tier 1qps limit.
        self.rate_limit.wait().await;
        let count = max_results.max(1).min(20);
        let resp = self
            .client
            .get(BRAVE_ENDPOINT)
            .query(&[("q", query), ("count", &count.to_string())])
            .header("X-Subscription-Token", &self.api_key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("network: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // 401/403 → bad/missing key. Surface as the actionable
            // cause rather than a generic HTTP error.
            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(ApiError::Storage(format!(
                    "Brave returned HTTP {} (key invalid or quota exhausted); \
                     check Settings → Search. Body: {}",
                    status.as_u16(),
                    truncate(&body, 200),
                )));
            }
            return Err(ApiError::Storage(format!(
                "Brave returned HTTP {}: {}",
                status.as_u16(),
                truncate(&body, 200),
            )));
        }
        let parsed: BraveResponse = resp
            .json()
            .await
            .map_err(|e| ApiError::Storage(format!("parsing JSON response: {e}")))?;
        let cap = max_results.max(1) as usize;
        let mut out = Vec::with_capacity(cap);
        if let Some(web) = parsed.web {
            for r in web.results.into_iter().take(cap) {
                if r.url.is_empty() || r.title.is_empty() {
                    continue;
                }
                out.push(SearchResult {
                    title: r.title,
                    url: r.url,
                    snippet: if r.description.is_empty() {
                        None
                    } else {
                        Some(r.description)
                    },
                });
            }
        }
        Ok(out)
    }
}

fn truncate(s: &str, max: usize) -> String {
    let trimmed: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!("{trimmed}…")
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_id_is_brave() {
        assert_eq!(BraveSearchApi::new("k").provider_id(), "brave");
    }

    #[tokio::test]
    async fn empty_key_returns_validation_error_not_panic() {
        let api = BraveSearchApi::new("");
        let err = api.search("anything", 10).await.unwrap_err();
        match err {
            ApiError::Validation(msg) => assert!(msg.contains("api_key")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parses_canned_brave_json_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = r#"{
            "web": {
                "results": [
                    {"title": "Brave first", "url": "https://example.com/a", "description": "First snippet"},
                    {"title": "Brave second", "url": "https://example.com/b", "description": ""}
                ]
            }
        }"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            sock.write_all(response.as_bytes()).await.unwrap();
        });
        // Build a client whose base URL is the mock listener but
        // shares the rest of the construction. Easier than trying
        // to override the const endpoint at compile time — just
        // hand the API a test-only client that resolves the same
        // path against the local mock by setting Host headers.
        // Implementation detail: we hit `/res/v1/web/search` on
        // the mock server, since the api appends that to the
        // hardcoded BRAVE_ENDPOINT — so swap in a dedicated
        // "stub Brave" by pointing the Client at the listener
        // through a manual reqwest call rather than through this
        // adapter. We test the parse logic via the public path
        // by constructing a local-loopback URL through the
        // `with_client` indirection... actually simpler: skip the
        // network altogether and parse directly. The
        // `BraveResponse` shape is `serde::Deserialize`; round-
        // trip JSON-string → Vec<SearchResult> by directly
        // calling the parse step.
        let parsed: BraveResponse = serde_json::from_str(body).unwrap();
        let web = parsed.web.unwrap();
        assert_eq!(web.results.len(), 2);
        assert_eq!(web.results[0].url, "https://example.com/a");
        assert_eq!(web.results[1].description, "");
        // Drop the listener (mock not actually consumed in this
        // simplified test).
        let _ = addr;
    }
}
