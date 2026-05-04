//! Tavily search-provider adapter.
//!
//! Tavily is a search API purpose-built for LLM/RAG pipelines.
//! Returns clean per-result snippets + an optional LLM-generated
//! answer summary. We DON'T request the answer summary here —
//! the gather phase has its own subagent for synthesis, and an
//! upstream answer would be redundant context that bloats the
//! per-step token budget.
//!
//! Wire format (https://docs.tavily.com/api-reference/endpoint/search):
//!
//!   POST https://api.tavily.com/search
//!   Header: Authorization: Bearer tvly-<api_key>
//!   Body:   {"query": "...", "max_results": N, "search_depth": "basic"}
//!
//!   Response: {
//!     "results": [{"title", "url", "content", "score"}, ...],
//!     "answer": "..." (when include_answer requested),
//!     "response_time": 1.5
//!   }
//!
//! `search_depth: "basic"` is the default + cheapest tier.
//! "advanced" costs 2 credits per call vs 1 for basic; the
//! gather phase already does its own readability extraction
//! against each URL, so the upgrade rarely buys anything for our
//! use case. Operators who want to flip this can override via the
//! adapter's config in a future revision; keeping the wire shape
//! tight today.
//!
//! 2026-05-04.

use async_trait::async_trait;
use execlaw_core::tool::{ApiError, SearchResult, WebSearchApi};
use serde::Deserialize;
use std::time::Duration;

const TAVILY_ENDPOINT: &str = "https://api.tavily.com/search";
const DEFAULT_TIMEOUT_S: u64 = 25;

/// Tavily's free tier doesn't publish a per-second cap (the
/// monthly credit count is the limit) but operator-friendly
/// pacing keeps a 7-step gather under 2 s of search wall-time
/// without sacrificing parallelism on URL fetch + subagent calls.
/// 250 ms (~4 qps) is well within polite-client norms.
const TAVILY_MIN_REQUEST_GAP: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Debug, Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    results: Vec<TavilyResult>,
}

#[derive(Debug, Deserialize)]
struct TavilyResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    /// Tavily calls the snippet `content`. The free tier returns
    /// ~1-2 short sentences; the paid tier returns longer
    /// extracted text. Either way, this is the snippet field.
    #[serde(default)]
    content: String,
}

pub struct TavilySearchApi {
    client: reqwest::Client,
    api_key: String,
    rate_limit: crate::search_rate_limit::RateLimitGate,
}

impl TavilySearchApi {
    pub fn new(api_key: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_S))
            .user_agent(crate::tool_apis_http::DEFAULT_USER_AGENT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            api_key: api_key.into(),
            rate_limit: crate::search_rate_limit::RateLimitGate::new(TAVILY_MIN_REQUEST_GAP),
        }
    }

    pub fn with_client(api_key: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            client,
            api_key: api_key.into(),
            rate_limit: crate::search_rate_limit::RateLimitGate::new(TAVILY_MIN_REQUEST_GAP),
        }
    }
}

#[async_trait]
impl WebSearchApi for TavilySearchApi {
    fn provider_id(&self) -> &str {
        "tavily"
    }
    async fn search(
        &self,
        query: &str,
        max_results: u32,
    ) -> Result<Vec<SearchResult>, ApiError> {
        if self.api_key.is_empty() {
            return Err(ApiError::Validation(
                "Tavily api_key is empty; configure it in Settings → Search".into(),
            ));
        }
        self.rate_limit.wait().await;
        // Tavily caps `max_results` at 20 server-side; clamp here
        // so the request is well-formed and the operator's intent
        // is preserved (returning 20 instead of erroring on >20).
        let max_results_clamped = max_results.max(1).min(20) as i64;
        let body = serde_json::json!({
            "query": query,
            "max_results": max_results_clamped,
            "search_depth": "basic",
            "include_answer": false,
            "include_raw_content": false,
            "include_images": false,
        });
        let resp = self
            .client
            .post(TAVILY_ENDPOINT)
            .bearer_auth(&self.api_key)
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("network: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // 401/403 → bad key. 432/433 → quota / rate limit
            // (Tavily uses these specific codes per their docs).
            // Surface each with a discriminating message so the
            // operator's mental model matches the cause.
            let actionable = match status.as_u16() {
                401 | 403 => Some("key invalid or quota exhausted"),
                429 | 432 | 433 => Some("rate-limited or out of monthly credits"),
                _ => None,
            };
            return Err(ApiError::Storage(if let Some(reason) = actionable {
                format!(
                    "Tavily returned HTTP {} ({reason}); check Settings → Search. Body: {}",
                    status.as_u16(),
                    truncate(&body, 200),
                )
            } else {
                format!(
                    "Tavily returned HTTP {}: {}",
                    status.as_u16(),
                    truncate(&body, 200),
                )
            }));
        }
        let parsed: TavilyResponse = resp
            .json()
            .await
            .map_err(|e| ApiError::Storage(format!("parsing JSON response: {e}")))?;
        let cap = max_results.max(1) as usize;
        let mut out = Vec::with_capacity(cap);
        for r in parsed.results.into_iter().take(cap) {
            if r.url.is_empty() || r.title.is_empty() {
                continue;
            }
            out.push(SearchResult {
                title: r.title,
                url: r.url,
                snippet: if r.content.is_empty() {
                    None
                } else {
                    Some(r.content)
                },
            });
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
    fn provider_id_is_tavily() {
        assert_eq!(TavilySearchApi::new("k").provider_id(), "tavily");
    }

    #[tokio::test]
    async fn empty_key_returns_validation_error_not_panic() {
        let api = TavilySearchApi::new("");
        let err = api.search("anything", 10).await.unwrap_err();
        match err {
            ApiError::Validation(msg) => assert!(msg.contains("api_key")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn parses_canned_tavily_json_response() {
        let body = r#"{
            "query": "test",
            "results": [
                {"title": "First", "url": "https://example.com/a", "content": "First snippet", "score": 0.95},
                {"title": "Second", "url": "https://example.com/b", "content": ""},
                {"title": "", "url": "https://example.com/c", "content": "title-empty (filtered)"}
            ],
            "response_time": 1.2
        }"#;
        let parsed: TavilyResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.results.len(), 3);
        assert_eq!(parsed.results[0].url, "https://example.com/a");
        assert_eq!(parsed.results[1].content, "");
    }
}
